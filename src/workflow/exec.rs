//! The workflow interpreter: `run_steps` (the router/step-list driver) and
//! `execute_step` (a single node's own action; its `on_error`/retry/timeout
//! handling lives in [`retry`], and what it needs to resolve before running
//! lives in [`settings`]). Router control flow and output aggregation live
//! in `routers`; model calls go through `crate::engine`.

use std::{future::Future, ops::ControlFlow, pin::Pin};

use anyhow::{Context, Result, bail};

use crate::{async_io, engine::RunContext, jq, template, workflow};

use super::WorkflowScope;

mod nodes;
mod retry;
mod routers;
mod settings;

pub(crate) use retry::{effective_retry, effective_timeout};
use retry::{execute_step_with_retry, run_on_error_handler};
pub(crate) use settings::resolve_step_settings;
use settings::{StepContext, resolve_attachments};

/// Attaches the `step '<label>'` context every step-level error carries, so
/// a failure inside `run_steps`/`execute_step`/a router always names which
/// step it happened in. `.step(label)` replaces the repeated
/// `.with_context(|| format!("step '{label}'"))` this file and `routers`
/// otherwise write at every fallible call.
pub(super) trait StepContextExt {
    fn step(self, label: &str) -> Self;
}

impl<T> StepContextExt for Result<T> {
    fn step(self, label: &str) -> Self {
        self.with_context(|| format!("step '{label}'"))
    }
}

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
/// The single wording for every "a workflow's cancellation token fired"
/// check in this module — [`check_workflow_cancellation`] and
/// [`wait_retry_delay`]'s own zero-delay check and `select!` branch.
const WORKFLOW_EXECUTION_CANCELLED: &str = "workflow execution was cancelled";

fn check_workflow_cancellation(
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    if cancellation.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        bail!(crate::error::cancelled(WORKFLOW_EXECUTION_CANCELLED));
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
                .step(&label)?;
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
                        match run_on_error_handler(step, error, counter, state, &router_context)
                            .await?
                        {
                            ControlFlow::Break(outcome) => return Ok(outcome),
                            ControlFlow::Continue(new_state) => state = new_state,
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

/// Runs a single node (agent call, prompt call, sub-workflow, command, or
/// `jq`/`write_file`-only data transform) and returns its output, with `jq`
/// applied afterward if set. `label` is the calling `use:` site's label,
/// used only for progress output/error messages. The per-node-type action
/// itself lives in `nodes::execute` — this wrapper adds only what applies
/// uniformly across every node type: `jq`/`write_file`.
async fn execute_step(
    node: &workflow::NodeDefinition,
    current_input: &str,
    context: StepContext<'_>,
) -> Result<String> {
    // Cloned before `context` is moved into `nodes::execute` below — cheap
    // (borrowed fields are `Copy`, `step_cancel` is an `Option<CancellationToken>`
    // clone) and lets this wrapper keep what it needs for the jq/write_file
    // tail without `nodes::execute` having to hand any of it back.
    let StepContext {
        env,
        label,
        steps_outputs,
        step_cancel,
        ..
    } = context.clone();

    let mut step_output = nodes::execute(node, current_input, context).await?;

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
        .step(label)?;
    }

    if let Some(path) = settings.write_file {
        async_io::write_output_file(path, &step_output, step_cancel)
            .await
            .step(label)?;
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
