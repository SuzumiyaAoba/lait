//! The workflow interpreter: `run_steps` (the router/step-list driver) and
//! `execute_step`/`execute_step_with_retry` (a single node's own action, plus
//! its retry/timeout handling). Router control flow and output aggregation
//! live in `routers`; model calls go through `crate::engine`.

use std::{
    borrow::Cow,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    agent::AgentFile,
    async_io, attachment,
    config::{self, ConfigFile},
    engine::{
        AgentTurn, CapabilityOverrides, PromptTurn, RequestSettings, RunContext, SamplingOverrides,
        call_agent, resolve_request_settings,
    },
    jq, response, schema, template, workflow,
};

use super::WorkflowScope;

mod routers;

/// Prints the `<prefix> name: description` announcement line shared by
/// `run_agent`/`run_workflow` (prefix `==>`) and `execute_step`'s `workflow:`
/// branch (a progress-indented `->`): nothing when `name` is unset, and no
/// trailing `:` when `description` is.
pub(crate) fn announce_named_file(prefix: &str, name: Option<&str>, description: Option<&str>) {
    let Some(name) = name else { return };
    match description {
        Some(description) => eprintln!("{prefix} {name}: {description}"),
        None => eprintln!("{prefix} {name}"),
    }
}

/// Sets `steps_outputs[step.label()]` to `output` (JSON-parsed, like a
/// `parallel` branch's output before joining). `FlowStep::label` is the
/// site's explicit `id`, else the node id it `use`s, else `None` for a
/// router site with no `id` — that case keeps the auto-generated `step-N`
/// progress label out of `{{ steps.* }}`/`$steps`, since that label isn't a
/// stable name to reference.
fn record_step_output(
    steps_outputs: &mut workflow::StepOutputs,
    step: &workflow::FlowStep,
    output: &str,
) {
    if let Some(key) = step.label() {
        steps_outputs.insert(key.to_string(), template::parse_input(output));
    }
}

/// A signal returned by `run_steps`, alongside its final input and progress
/// counter, describing how the run ended: `Continue` is the normal
/// end-of-list case; `Break`/`Stop` come from a `break: true`/`stop: true`
/// step (see `workflow::FlowStep`) and bubble up through `switch`/
/// `loop`/`for_each` frames until something catches them (`loop`/`for_each`
/// catch `Break`; nothing but `run_workflow` catches `Stop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    Break,
    Stop,
}

/// The final input, the running progress counter, the `Flow` signal the run
/// ended with, and the named step outputs recorded along the way, returned by
/// `run_steps`.
pub(crate) struct StepsOutcome {
    pub(crate) output: String,
    pub(crate) counter: usize,
    pub(crate) flow: Flow,
    pub(crate) steps_outputs: workflow::StepOutputs,
}

impl StepsOutcome {
    fn into_state(self) -> StepsState {
        StepsState {
            output: self.output,
            counter: self.counter,
            steps_outputs: self.steps_outputs,
        }
    }
}

/// Mutable data threaded through one sequential step list. Keeping it
/// separate from [`RunStepsFrame`] makes router helpers explicit about what
/// they transform (the current value, progress counter, and named outputs)
/// versus the read-only execution environment they borrow.
struct StepsState {
    output: String,
    counter: usize,
    steps_outputs: workflow::StepOutputs,
}

impl StepsState {
    fn record_output(&mut self, step: &workflow::FlowStep) {
        record_step_output(&mut self.steps_outputs, step, &self.output);
    }

    fn into_outcome(self, flow: Flow) -> StepsOutcome {
        StepsOutcome {
            output: self.output,
            counter: self.counter,
            flow,
            steps_outputs: self.steps_outputs,
        }
    }
}

/// Physical concurrency inherited across workflow-file boundaries. Lexical
/// break/stop scopes are validated separately within each workflow document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ExecutionPlacement {
    #[default]
    Sequential,
    Parallel,
    ConcurrentItems,
}

impl ExecutionPlacement {
    fn parallel(self) -> Self {
        match self {
            Self::ConcurrentItems => self,
            Self::Sequential | Self::Parallel => Self::Parallel,
        }
    }
}

fn validate_node_placement(
    node: &workflow::NodeDefinition,
    placement: ExecutionPlacement,
    label: &str,
) -> Result<()> {
    if placement != ExecutionPlacement::Sequential && node.requires_interactive_stdin() {
        bail!(
            "step '{label}' cannot run an 'ask' node inside concurrent execution, including a nested workflow"
        );
    }
    if placement == ExecutionPlacement::ConcurrentItems && node.settings().write_file.is_some() {
        bail!(
            "step '{label}' has 'write_file' inside a concurrent 'for_each', including a nested workflow; move the write after the loop"
        );
    }
    Ok(())
}

/// Preflights a newly loaded child before any of its actions run. References
/// to other workflow files remain lazy; each child checks its own plan on load.
fn validate_execution_placement(
    steps: &[workflow::FlowStep],
    placement: ExecutionPlacement,
) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        if let Some(call) = step.call() {
            validate_node_placement(call.definition, placement, &step.label_or(index + 1))?;
        }
        if let Some(handler) = step.on_error() {
            validate_execution_placement(&handler.steps, placement)?;
        }
        match step.router() {
            Some(workflow::Router::Switch(router)) => {
                for case in &router.cases {
                    validate_execution_placement(&case.steps, placement)?;
                }
                if let Some(steps) = &router.else_steps {
                    validate_execution_placement(steps, placement)?;
                }
            }
            Some(workflow::Router::Parallel(router)) => {
                for branch in &router.branches {
                    validate_execution_placement(&branch.steps, placement.parallel())?;
                }
            }
            Some(workflow::Router::Loop(router)) => {
                validate_execution_placement(&router.steps, placement)?
            }
            Some(workflow::Router::ForEach(router)) => {
                let inner = if router.max_concurrency.unwrap_or(1) > 1 {
                    ExecutionPlacement::ConcurrentItems
                } else {
                    placement
                };
                validate_execution_placement(&router.steps, inner)?;
            }
            None => {}
        }
    }
    Ok(())
}

/// Read-only context shared by all router handlers. `RunStepsFrame` keeps the
/// caller's starting counter because it is part of the public recursive entry
/// point; router handlers only need the invariant scope/environment/prefix and
/// the cancellation token, so this smaller view avoids passing unrelated state
/// through every control-structure helper.
struct RouterContext<'a> {
    scope: &'a WorkflowScope,
    env: &'a RunContext,
    placement: ExecutionPlacement,
    progress_prefix: &'a str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
}

impl<'a> RouterContext<'a> {
    fn frame<'b>(&'b self, start_counter: usize, progress_prefix: &'b str) -> RunStepsFrame<'b> {
        RunStepsFrame {
            scope: self.scope,
            env: self.env,
            start_counter,
            progress_prefix,
            cancellation: self.cancellation.clone(),
            placement: self.placement,
        }
    }
}

/// Returns an error as soon as the cancellation inherited from an enclosing
/// timed step/workflow is observed. Router frames use this check between
/// child operations as well as passing the receiver into jq itself, so a
/// cancellation cannot be lost merely because a router has no model node of
/// its own.
fn check_workflow_cancellation(
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    if cancellation.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        bail!(crate::error::Interrupted::cancelled(
            "workflow execution was cancelled"
        ));
    }
    Ok(())
}

/// Execution scope, cancellation, and progress for a sequence of steps.
/// Routers inherit this frame, adjusting progress and placement for concurrent
/// branches. Child workflows replace the scope while retaining the parent's
/// concurrency restrictions and the calling node's cancellation token.
pub(crate) struct RunStepsFrame<'a> {
    pub(crate) scope: &'a WorkflowScope,
    pub(crate) env: &'a RunContext,
    pub(crate) placement: ExecutionPlacement,
    pub(crate) start_counter: usize,
    pub(crate) progress_prefix: &'a str,
    pub(crate) cancellation: Option<tokio_util::sync::CancellationToken>,
}

impl<'a> RunStepsFrame<'a> {
    fn with_placement(mut self, placement: ExecutionPlacement) -> Self {
        self.placement = placement;
        self
    }
}

/// Runs a sequence of steps (the workflow's top-level `steps`, the nested
/// `steps` of a `switch` case/`else`, or a `parallel` branch), returning the
/// final input and the running progress counter so nested calls keep
/// numbering `[n]` labels continuously across the whole executed path
/// (skipped steps still consume a number). `frame.progress_prefix` is
/// prepended to every progress line, so a `parallel` branch's interleaved
/// output stays attributable to its branch; it is threaded through unchanged
/// by `switch` (only one case ever runs, so its numbering stays continuous
/// with the parent) but reset to a fresh branch-local prefix and counter by
/// `parallel` (every branch runs concurrently, so a single shared counter
/// would not reflect real execution order). `steps_outputs` is threaded the
/// same way as `current_input`/`counter` for a `switch` case, `loop`
/// iteration, or `for_each` item (each sees every id recorded so far, and its
/// own recordings flow to whatever runs after it), but is only ever cloned
/// into a `parallel` branch, never merged back: concurrently running branches
/// recording into a shared namespace would race, and there is no well-defined
/// "the" value for an id set differently by two branches. Boxed because a
/// `switch`/`parallel` step recurses into this function from within an
/// `async` body, which Rust cannot size otherwise. `frame.cancellation` is
/// cloned into every nested frame and router jq operation, preserving the
/// timeout of the enclosing step/workflow across control-flow boundaries.
pub(crate) fn run_steps<'a>(
    steps: &'a [workflow::FlowStep],
    current_input: String,
    steps_outputs: workflow::StepOutputs,
    frame: RunStepsFrame<'a>,
) -> Pin<Box<dyn Future<Output = Result<StepsOutcome>> + Send + 'a>> {
    let RunStepsFrame {
        scope,
        env,
        start_counter,
        progress_prefix,
        cancellation,
        placement,
    } = frame;
    Box::pin(async move {
        let mut state = StepsState {
            output: current_input,
            counter: start_counter,
            steps_outputs,
        };
        let router_context = RouterContext {
            scope,
            env,
            placement,
            progress_prefix,
            cancellation: cancellation.clone(),
        };
        for step in steps {
            check_workflow_cancellation(cancellation.as_ref())?;
            state.counter += 1;
            let counter = state.counter;
            let label = step.label_or(counter);

            if let Some(router) = step.router() {
                eprintln!("{progress_prefix}[{counter}] {label}");
                let outcome =
                    routers::execute(router, step, state, &router_context, &label).await?;
                if outcome.flow != Flow::Continue {
                    return Ok(outcome);
                }
                state = outcome.into_state();
                continue;
            }

            if let Some(when) = step.when() {
                let truthy = workflow::eval_when_async(
                    when,
                    &state.output,
                    &state.steps_outputs,
                    &env.vars,
                    cancellation.clone(),
                )
                .await
                .with_context(|| format!("step '{label}'"))?;
                if !truthy {
                    eprintln!("{progress_prefix}[{counter}] {label} (skipped)");
                    continue;
                }
            }

            eprintln!("{progress_prefix}[{counter}] {label}");
            if let Some(call) = step.call() {
                validate_node_placement(call.definition, placement, &label)?;
                let attempt_result = execute_step_with_retry(
                    call.definition,
                    &state.output,
                    StepContext {
                        scope,
                        env,
                        placement,
                        label: &label,
                        progress_prefix,
                        steps_outputs: &state.steps_outputs,
                        step_cancel: cancellation.clone(),
                    },
                )
                .await;
                match attempt_result {
                    Ok(output) => state.output = output,
                    Err(error) => {
                        let Some(on_error) = step.on_error() else {
                            return Err(error);
                        };
                        eprintln!(
                            "{progress_prefix}    -> step failed, running 'on_error': {error}"
                        );
                        let error_input = serde_json::json!({
                            "error": format!("{error:#}"),
                            "input": template::parse_input(&state.output),
                        });
                        let error_input_json = serde_json::to_string(&error_input)
                            .context("failed to serialize 'on_error' input")?;
                        let outcome = run_steps(
                            &on_error.steps,
                            error_input_json,
                            state.steps_outputs,
                            RunStepsFrame {
                                scope,
                                env,
                                start_counter: counter,
                                progress_prefix,
                                cancellation: cancellation.clone(),
                                placement,
                            },
                        )
                        .await?;
                        let flow = outcome.flow;
                        state = outcome.into_state();
                        if flow != Flow::Continue {
                            // A handler's Break/Stop completes the failed site
                            // before propagating to the enclosing control scope.
                            state.record_output(step);
                            return Ok(state.into_outcome(flow));
                        }
                    }
                }
            }

            state.record_output(step);
            match step.control() {
                workflow::Control::Continue => {}
                workflow::Control::Break => return Ok(state.into_outcome(Flow::Break)),
                workflow::Control::Stop => return Ok(state.into_outcome(Flow::Stop)),
            }
        }
        Ok(state.into_outcome(Flow::Continue))
    })
}

/// The upper bound on a single wait between retry attempts (see
/// `execute_step_with_retry`): a `retry` whose `delay_seconds`/`backoff`
/// (validated non-negative and finite by `workflow::validate`, but free to
/// grow exponentially) would wait longer than this waits this long instead —
/// a bounded, predictable worst case rather than an arbitrarily long hang.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(3600);

/// The `retry` actually in effect for `node`: its own `retry` if set, else
/// (only for a node that calls a model — see `NodeDefinition::calls_model`)
/// `scope`'s `defaults.retry`. Shared by `execute_step_with_retry`, which
/// runs under it, and `dryrun::print_plan`, which only displays it (`lait run
/// --dry-run`).
pub(crate) fn effective_retry<'a>(
    node: &'a workflow::NodeDefinition,
    scope: &'a WorkflowScope,
) -> Option<&'a workflow::RetryDefinition> {
    let settings = node.settings();
    settings.retry.or(node
        .calls_model()
        .then_some(scope.defaults.retry.as_ref())
        .flatten())
}

/// The `timeout` actually in effect for `node`, under the same node-first,
/// model-calling-only fallback rule as [`effective_retry`].
pub(crate) fn effective_timeout(
    node: &workflow::NodeDefinition,
    scope: &WorkflowScope,
) -> Option<u64> {
    let settings = node.settings();
    settings.timeout.or(node
        .calls_model()
        .then_some(scope.defaults.timeout)
        .flatten())
}

/// Runs `execute_step`, applying an effective timeout to each attempt and
/// retrying per an effective `retry` on failure (a timed-out attempt counts
/// as a failure). "Effective" means the node's own `retry`/`timeout` if set,
/// else `scope`'s `defaults.retry`/`defaults.timeout` (see
/// `WorkflowScope::defaults`) — but only for a node that calls a model
/// (`prompt`/`system_prompt`/`agent`, see `NodeDefinition::calls_model`): a
/// `jq`-only or `workflow:` node never falls back to the workflow default
/// (a `workflow:` node's own `retry`/`timeout` are
/// rejected by `validate::validate_node` in favor of the sub-workflow's own
/// steps setting theirs, and applying the *caller's* default on top of that
/// would double up whatever the sub-workflow's own steps already inherit).
/// Returns the last attempt's error once the effective `max_attempts` (or 1,
/// with no effective `retry`) is exhausted; the caller decides whether to run
/// `on_error` or propagate it. `label` is the calling `use:` site's label
/// (not the node's own id), so error messages point at where in the flow the
/// failure happened.
async fn execute_step_with_retry(
    node: &workflow::NodeDefinition,
    current_input: &str,
    context: StepContext<'_>,
) -> Result<String> {
    let StepContext {
        scope,
        env,
        placement,
        label,
        progress_prefix,
        steps_outputs,
        step_cancel: workflow_cancel,
    } = context;

    let effective_retry = effective_retry(node, scope);
    let effective_timeout = effective_timeout(node, scope);

    let max_attempts = effective_retry
        .and_then(|retry| retry.max_attempts)
        .unwrap_or(1);
    let backoff = effective_retry
        .and_then(|retry| retry.backoff)
        .unwrap_or(1.0);
    let mut delay = Duration::from_secs(
        effective_retry
            .and_then(|retry| retry.delay_seconds)
            .unwrap_or(0),
    )
    .min(MAX_RETRY_DELAY);

    let mut attempt = 0usize;
    loop {
        attempt += 1;
        check_workflow_cancellation(workflow_cancel.as_ref())?;
        tracing::debug!(step = %label, attempt, max_attempts, "step started");
        let outcome = match effective_timeout {
            // Keep the timeout around the whole node action (including its
            // later jq/write_file work). A cancellation channel is passed to
            // every timed node, not just command nodes: jq and write_file
            // also run outside Tokio and must be told to stop waiting before
            // a retry or an on_error branch starts. The future stays borrowed
            // until cancellation cleanup finishes, avoiding a second attempt
            // racing the child-owning or file-writing future.
            Some(seconds) => {
                // A child token is cancelled both by this node's own timeout
                // (below) and by `workflow_cancel` being cancelled (a
                // `CancellationToken` property, not something forwarded by
                // hand) — `execute_step` only ever needs to watch this one
                // token either way.
                let node_cancel = match &workflow_cancel {
                    Some(parent) => parent.child_token(),
                    None => tokio_util::sync::CancellationToken::new(),
                };
                let execution = execute_step(
                    node,
                    current_input,
                    StepContext {
                        scope,
                        env,
                        placement,
                        label,
                        progress_prefix,
                        steps_outputs,
                        step_cancel: Some(node_cancel.clone()),
                    },
                );
                tokio::pin!(execution);
                match tokio::time::timeout(Duration::from_secs(seconds), &mut execution).await {
                    Ok(result) => result,
                    Err(_) => {
                        node_cancel.cancel();
                        let _ = execution.await;
                        Err(anyhow!(crate::error::Interrupted::timed_out(format!(
                            "step '{label}' timed out after {seconds}s (attempt {attempt}/{max_attempts})"
                        ))))
                    }
                }
            }
            None => {
                execute_step(
                    node,
                    current_input,
                    StepContext {
                        scope,
                        env,
                        placement,
                        label,
                        progress_prefix,
                        steps_outputs,
                        step_cancel: workflow_cancel.clone(),
                    },
                )
                .await
            }
        };

        match outcome {
            Ok(output) => {
                tracing::debug!(step = %label, attempt, "step finished");
                return Ok(output);
            }
            Err(error) if attempt < max_attempts => {
                check_workflow_cancellation(workflow_cancel.as_ref())?;
                tracing::debug!(
                    step = %label,
                    attempt,
                    max_attempts,
                    error = %error,
                    delay_secs = delay.as_secs_f64(),
                    "step retrying",
                );
                eprintln!(
                    "{progress_prefix}    -> attempt {attempt}/{max_attempts} failed: {error}; retrying in {:.1}s",
                    delay.as_secs_f64()
                );
                wait_retry_delay(delay, workflow_cancel.as_ref()).await?;
                // `try_from_secs_f64` + the `MAX_RETRY_DELAY` clamp keep an
                // exponentially growing (or pathological) delay from
                // overflowing `Duration` — `Duration::from_secs_f64` would
                // panic there instead of just waiting the capped hour.
                delay = Duration::try_from_secs_f64((delay.as_secs_f64() * backoff).max(0.0))
                    .unwrap_or(MAX_RETRY_DELAY)
                    .min(MAX_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Sleeps between retries while still honoring cancellation inherited from a
/// surrounding workflow. A plain `sleep` would allow a cancelled nested
/// workflow to wait for an arbitrarily large backoff before returning.
async fn wait_retry_delay(
    delay: Duration,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    let Some(cancellation) = cancellation else {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        return Ok(());
    };
    if delay.is_zero() {
        if cancellation.is_cancelled() {
            bail!(crate::error::Interrupted::cancelled(
                "workflow execution was cancelled"
            ));
        }
        return Ok(());
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => bail!(crate::error::Interrupted::cancelled("workflow execution was cancelled")),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

/// The state a single node execution needs, bundled so
/// `execute_step_with_retry`/`execute_step` take one parameter instead of
/// six. `step_cancel` is the cancellation in effect for this particular
/// attempt — the caller's own cancellation on the first attempt of a node
/// with no `timeout`, or a child token scoped to just that attempt when a
/// `timeout` is set (see `execute_step_with_retry`) — which is why it lives
/// here rather than on `RunContext`: it changes across attempts and nesting
/// depths, unlike everything on `RunContext`, which does not.
#[derive(Clone)]
struct StepContext<'a> {
    scope: &'a WorkflowScope,
    env: &'a RunContext,
    placement: ExecutionPlacement,
    label: &'a str,
    progress_prefix: &'a str,
    steps_outputs: &'a workflow::StepOutputs,
    step_cancel: Option<tokio_util::sync::CancellationToken>,
}

/// Resolves the model/reasoning-effort settings for a node's model call,
/// applying the node > agent file (when this node has one) > workflow
/// default precedence chain shared by `execute_step`'s `agent` and `prompt`
/// branches. `agent_file` is `Some` only for an `agent` node; besides adding
/// its fallback layer, its presence also selects which hint text a
/// missing-model error uses. Also called (read-only, no network) by
/// `dryrun::print_plan` to display the resolved model/base_url for `lait run
/// --dry-run`.
pub(crate) fn resolve_step_settings(
    node: &workflow::NodeDefinition,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    agent_file: Option<&AgentFile>,
    label: &str,
) -> Result<RequestSettings> {
    let node_settings = node.settings();
    let model_name = node_settings
        .model
        .map(str::to_owned)
        .or_else(|| agent_file.and_then(|agent_file| agent_file.model.clone()))
        .or_else(|| scope.defaults.model.clone())
        .or_else(|| file_config.default.model.clone())
        .ok_or_else(|| {
            anyhow!(
                "model is required for step '{label}'; set it on the node,{} the workflow's default.model, or in {}",
                if agent_file.is_some() { " its agent file," } else { "" },
                config::CONFIG_FILE_NAME
            )
        })?;
    // Each layer mirrors one of the node > agent file > workflow default
    // precedence chain's own sources; `agent_file`'s is absent (`Default`,
    // all `None`/empty) when this node has none. `SamplingOverrides::fold`/
    // `CapabilityOverrides::fold` then pick the first layer with each field
    // set, independently per field.
    let node_sampling = SamplingOverrides {
        reasoning_effort: node_settings.reasoning_effort,
        temperature: node_settings.temperature,
        top_p: node_settings.top_p,
        max_tokens: node_settings.max_tokens,
    };
    let agent_sampling = agent_file
        .map(|agent_file| SamplingOverrides {
            reasoning_effort: agent_file.reasoning_effort,
            temperature: agent_file.temperature,
            top_p: agent_file.top_p,
            max_tokens: agent_file.max_tokens,
        })
        .unwrap_or_default();
    let workflow_sampling = SamplingOverrides {
        reasoning_effort: scope.defaults.reasoning_effort,
        temperature: scope.defaults.temperature,
        top_p: scope.defaults.top_p,
        max_tokens: scope.defaults.max_tokens,
    };
    let overrides = SamplingOverrides::fold(&[node_sampling, agent_sampling, workflow_sampling]);

    let node_capability = CapabilityOverrides {
        mcp: node_settings.mcp.map(<[String]>::to_vec),
        max_tool_rounds: node_settings.max_tool_rounds,
        skills: node_settings.skills.map(<[String]>::to_vec),
        subagents: node_settings.subagents.map(<[String]>::to_vec),
        tools: node_settings.tools.map(<[String]>::to_vec),
    };
    let agent_capability = agent_file
        .map(|agent_file| CapabilityOverrides {
            mcp: agent_file.mcp.clone(),
            max_tool_rounds: agent_file.max_tool_rounds,
            skills: agent_file.skills.clone(),
            subagents: agent_file.subagents.clone(),
            tools: agent_file.tools.clone(),
        })
        .unwrap_or_default();
    let workflow_capability = CapabilityOverrides {
        mcp: scope.defaults.mcp.clone(),
        max_tool_rounds: scope.defaults.max_tool_rounds,
        skills: scope.defaults.skills.clone(),
        subagents: scope.defaults.subagents.clone(),
        tools: scope.defaults.tools.clone(),
    };
    let capability_overrides =
        CapabilityOverrides::fold(&[node_capability, agent_capability, workflow_capability]);

    resolve_request_settings(
        model_name,
        overrides,
        None,
        None,
        capability_overrides,
        &scope.models,
        file_config,
    )
    .with_context(|| format!("step '{label}'"))
}

/// Resolves a node's `files:`/`images:` attachments against `base_prompt`:
/// file contents become a named fenced code block appended after it
/// (`base_prompt` unchanged when `files` is unset), and image paths/URLs
/// resolve into `image_url` content parts for the caller's eventual
/// `AgentTurn`/`PromptTurn`. The two kinds are read/resolved concurrently
/// since they're otherwise-independent I/O. Shared by `execute_step`'s
/// `Agent` and `Prompt` arms, which each attach to a different "base" user
/// message (the current input passed through unchanged, vs. the rendered
/// `prompt` template) — takes `files`/`images` directly (rather than a
/// `&workflow::NodeDefinition`) since only those two variants have either
/// field.
async fn resolve_attachments<'a>(
    files: Option<&[PathBuf]>,
    images: Option<&[String]>,
    base_prompt: &'a str,
    label: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<(Cow<'a, str>, Vec<String>)> {
    let (file_context, image_urls) = tokio::try_join!(
        attachment::read_file_attachments_cancellable(files.unwrap_or(&[]), cancellation.clone(),),
        attachment::resolve_image_urls_cancellable(images.unwrap_or(&[]), cancellation),
    )
    .with_context(|| format!("step '{label}'"))?;
    let prompt = match file_context {
        Some(context) => Cow::Owned(format!("{base_prompt}\n\n{context}")),
        None => Cow::Borrowed(base_prompt),
    };
    Ok((prompt, image_urls))
}

/// Runs a single node (agent call, prompt call, sub-workflow, command, or
/// `jq`/`write_file`-only data transform) and returns its output, with `jq`
/// applied afterward if set. `label` is the calling `use:` site's label,
/// used only for progress output/error messages.
async fn execute_step(
    node: &workflow::NodeDefinition,
    current_input: &str,
    context: StepContext<'_>,
) -> Result<String> {
    let StepContext {
        scope,
        env,
        placement,
        label,
        progress_prefix,
        steps_outputs,
        step_cancel,
    } = context;

    let mut step_output = match node {
        workflow::NodeDefinition::Prompt(prompt_node) => {
            if let Some(name_or_path) = &prompt_node.input_schema {
                let schema = schema::resolve_named_schema_value_cancellable(
                    &scope.json_schemas,
                    name_or_path,
                    step_cancel.clone(),
                )
                .await
                .with_context(|| format!("step '{label}'"))?;
                let input = template::parse_input(current_input);
                schema::validate_input_against_schema(&schema, &input)
                    .with_context(|| format!("step '{label}'"))?;
            }

            let settings =
                resolve_step_settings(node, scope, &env.services.file_config, None, label)?
                    .with_usage_label(label);

            let response_format = match prompt_node.output_schema.as_deref() {
                Some(name_or_path) => {
                    let schema_name = prompt_node
                        .schema_name
                        .as_deref()
                        .unwrap_or("structured_output");
                    let response_format = match scope.json_schemas.get(name_or_path) {
                        Some(entry) => {
                            schema::build_response_format_from_entry_cancellable(
                                entry,
                                schema_name,
                                step_cancel.clone(),
                            )
                            .await
                        }
                        None => {
                            schema::load_json_schema_cancellable(
                                Path::new(name_or_path),
                                schema_name,
                                step_cancel.clone(),
                            )
                            .await
                        }
                    };
                    Some(response_format.with_context(|| format!("step '{label}'"))?)
                }
                None => None,
            };

            let input = template::parse_input(current_input);
            // A `system_prompt`-only node (no `prompt`) sends the current
            // input unchanged as the user message, the same way an `agent`
            // node's `current_input` passes straight through `call_agent`
            // without going through `template::render`.
            let prompt: Cow<'_, str> = match &prompt_node.prompt {
                Some(prompt_template) => Cow::Owned(
                    template::render(prompt_template, &input, steps_outputs, &env.vars)
                        .with_context(|| format!("step '{label}'"))?,
                ),
                None => Cow::Borrowed(current_input),
            };
            let (prompt, image_urls) = resolve_attachments(
                prompt_node.files.as_deref(),
                prompt_node.images.as_deref(),
                &prompt,
                label,
                step_cancel.clone(),
            )
            .await?;
            let system_prompt = prompt_node
                .system_prompt
                .as_deref()
                .or(scope.defaults.system_prompt.as_deref())
                .map(|system_prompt_template| {
                    template::render(system_prompt_template, &input, steps_outputs, &env.vars)
                })
                .transpose()
                .with_context(|| format!("step '{label}'"))?;

            let response = settings
                .complete(
                    env,
                    &[],
                    PromptTurn {
                        system_prompt: system_prompt.as_deref(),
                        history: &[],
                        prompt: &prompt,
                        image_urls: &image_urls,
                    },
                    response_format,
                    step_cancel.clone(),
                )
                .await
                .with_context(|| format!("step '{label}'"))?;

            response::render_response(&response, false, false)
                .with_context(|| format!("step '{label}'"))?
        }
        workflow::NodeDefinition::Agent(agent_node) => {
            // Loaded through the registry's path cache (not
            // `agent::load_agent` directly) so a `for_each`/`loop` body
            // re-running this node reuses the parsed file and its resolved
            // input schema instead of re-reading both from disk on every
            // iteration.
            let loaded = env
                .services
                .agent_registry
                .load_path_cancellable(&agent_node.agent, step_cancel.clone())
                .await
                .with_context(|| format!("step '{label}'"))?;
            let agent_file = &loaded.file;

            let input = template::parse_input(current_input);
            loaded
                .validate_input(&input)
                .with_context(|| format!("step '{label}'"))?;

            let settings = resolve_step_settings(
                node,
                scope,
                &env.services.file_config,
                Some(agent_file),
                label,
            )?
            .with_usage_label(label);

            let (prompt, image_urls) = resolve_attachments(
                agent_node.files.as_deref(),
                agent_node.images.as_deref(),
                current_input,
                label,
                step_cancel.clone(),
            )
            .await?;

            call_agent(
                agent_file,
                &settings,
                env,
                AgentTurn {
                    input: &input,
                    prompt: &prompt,
                    image_urls: &image_urls,
                },
                steps_outputs,
                std::slice::from_ref(&loaded.canonical_path),
                step_cancel.clone(),
            )
            .await
            .with_context(|| format!("step '{label}'"))?
        }
        workflow::NodeDefinition::Workflow(workflow_node) => {
            // Resolve cycles before opening a child file: a recursive FIFO
            // reference must fail rather than waiting for a second writer.
            let resolved_path = scope
                .resolve_nested_path(&workflow_node.workflow, label, step_cancel.clone())
                .await?;
            let mut sub_wf =
                workflow::load_workflow_cancellable(&resolved_path, step_cancel.clone())
                    .await
                    .with_context(|| format!("step '{label}'"))?;
            validate_execution_placement(&sub_wf.steps, placement).with_context(|| {
                format!("step '{label}': workflow '{}'", resolved_path.display())
            })?;
            let sub_scope = scope.nested(resolved_path, &mut sub_wf);
            announce_named_file(
                &format!("{progress_prefix}    ->"),
                sub_wf.name.as_deref(),
                sub_wf.description.as_deref(),
            );
            // Isolated like an `agent:` call, not threaded like a `switch`
            // case: the sub-workflow is a separate file with its own step
            // ids, so it starts with an empty `steps_outputs` and its Flow
            // (whether it ended via `stop`/`break` internally or just ran
            // out of steps) is this step's own concern, not the caller's —
            // only its final output crosses back.
            let sub_progress_prefix = format!("{progress_prefix}    ");
            let StepsOutcome { output: result, .. } = run_steps(
                &sub_wf.steps,
                current_input.to_string(),
                workflow::StepOutputs::new(),
                RunStepsFrame {
                    scope: &sub_scope,
                    env,
                    start_counter: 0,
                    progress_prefix: &sub_progress_prefix,
                    cancellation: step_cancel.clone(),
                    placement,
                },
            )
            .await
            .with_context(|| format!("step '{label}'"))?;
            result
        }
        workflow::NodeDefinition::Command(command_node) => {
            let input = template::parse_input(current_input);
            let rendered_argv: Vec<String> = command_node
                .command
                .iter()
                .map(|arg| template::render(arg, &input, steps_outputs, &env.vars))
                .collect::<Result<_>>()
                .with_context(|| format!("step '{label}'"))?;
            crate::process::run_command(&rendered_argv, current_input, step_cancel.clone())
                .await
                .with_context(|| format!("step '{label}'"))?
        }
        workflow::NodeDefinition::Transform(_) => current_input.to_string(),
        workflow::NodeDefinition::Ask(ask_node) => {
            let input = template::parse_input(current_input);
            let prompt = template::render(&ask_node.prompt, &input, steps_outputs, &env.vars)
                .with_context(|| format!("step '{label}'"))?;
            super::ask::run_ask(&prompt, ask_node, step_cancel.clone())
                .await
                .with_context(|| format!("step '{label}'"))?
        }
    };

    let settings = node.settings();
    if let Some(filter) = settings.jq {
        step_output = apply_jq(
            filter,
            &step_output,
            steps_outputs,
            &env.vars,
            step_cancel.as_ref(),
        )
        .await
        .with_context(|| format!("step '{label}'"))?;
    }

    if let Some(path) = settings.write_file {
        async_io::write_output_file(path, &step_output, step_cancel)
            .await
            .with_context(|| format!("step '{label}'"))?;
    }

    Ok(step_output)
}

/// Applies a node's jq transform off the Tokio workers. jq evaluation is
/// synchronous and can be expensive for a large input; running it on a
/// dedicated OS thread means the enclosing node timeout remains effective.
/// The worker receives a cooperative cancellation flag and is awaited after a
/// timeout, so a cancelled evaluation does not continue as a detached thread
/// after the workflow attempt has moved on.
async fn apply_jq(
    filter: &str,
    input: &str,
    steps_outputs: &workflow::StepOutputs,
    vars: &workflow::StepOutputs,
    step_cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let cancellation = step_cancel.cloned();
    // Input normalization is deliberately performed inside the bounded jq
    // worker. A large plain-text model/command result must not be parsed and
    // re-serialized on a Tokio executor thread before cancellation can win.
    jq::apply_cancellable_async(filter, input, steps_outputs, vars, cancellation).await
}

#[cfg(test)]
mod tests {
    use super::{RunContext, RunStepsFrame, WorkflowScope, apply_jq, run_steps};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn an_already_cancelled_jq_stops_before_returning_a_value() {
        let token = CancellationToken::new();
        token.cancel();
        let steps = crate::workflow::StepOutputs::new();
        let vars = crate::workflow::StepOutputs::new();
        let started = std::time::Instant::now();

        // The filter is intentionally expensive if it is allowed to run. A
        // pre-set step cancellation must be observed immediately, before a
        // caller can mistake a value from the worker for a successful step.
        let result = apply_jq("range(0; 1000000000)", "null", &steps, &vars, Some(&token)).await;

        assert!(result.is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "pre-cancelled jq took too long to stop: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_router_condition_observes_the_enclosing_workflow_cancellation() {
        let path = crate::test_support::unique_temp_path("lait-router-cancel", ".yml");
        std::fs::write(
            &path,
            r#"
steps:
  - switch:
      cases:
        - when: 'reduce range(0; 100000000) as $i (false; .)'
          steps:
            - stop: true
      else:
        - stop: true
"#,
        )
        .expect("router workflow fixture should be writable");
        let mut workflow = crate::workflow::load_workflow(&path).unwrap();
        let scope = WorkflowScope::top_level(&mut workflow, &path, None)
            .await
            .unwrap();
        let config = std::sync::Arc::new(crate::config::ConfigFile::default());
        let env = RunContext::new(
            std::sync::Arc::new(crate::engine::AppServices::new(config)),
            tokio_util::sync::CancellationToken::new(),
        );
        let token = CancellationToken::new();
        let started = std::time::Instant::now();
        let execution = run_steps(
            &workflow.steps,
            "null".to_owned(),
            crate::workflow::StepOutputs::new(),
            RunStepsFrame {
                scope: &scope,
                env: &env,
                start_counter: 0,
                progress_prefix: "",
                cancellation: Some(token.clone()),
                placement: Default::default(),
            },
        );
        tokio::pin!(execution);
        let result = tokio::select! {
            result = &mut execution => result,
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                token.cancel();
                tokio::time::timeout(std::time::Duration::from_secs(2), &mut execution)
                    .await
                    .expect("cancelled router should stop promptly")
            }
        };
        let _ = std::fs::remove_file(path);
        assert!(result.is_err(), "a cancelled router must not succeed");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "router cancellation took too long: {:?}",
            started.elapsed()
        );
    }
}
