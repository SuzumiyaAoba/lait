//! The workflow interpreter. Values flowing between steps are JSON values:
//! text-producing actions yield strings, Structured Outputs and jq yield
//! whatever JSON they produce, and a value only becomes text where text is
//! needed (a user message, a command's stdin, a written file, the final
//! output) via [`template::to_text`].

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, TryStreamExt};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::AgentFile,
    async_io, attachment,
    config::{self, ConfigFile},
    engine::{
        AgentTurn, CapabilityOverrides, EndpointOverrides, PromptTurn, RequestSettings, RunContext,
        SamplingOverrides, call_agent, resolve_request_settings,
    },
    jq, response, schema, subagent, template,
};

use super::{
    StepOutputs, WorkflowScope, inputs,
    model::{
        AgentRef, AgentStep, Attachments, Child, DecideStep, ForEachStep, LlmOverrides,
        LoopCondition, LoopStep, ParallelStep, PromptStep, RetryPolicy, Step, StepKind, SwitchStep,
        WorkflowFile, WorkflowRef, WorkflowStep,
    },
};

/// Prints the `<prefix> name: description` announcement shared by
/// `lait run`/`lait agent run` (`==>`) and a nested `workflow:` step.
pub(crate) fn announce_named_file(prefix: &str, name: Option<&str>, description: Option<&str>) {
    let Some(name) = name else { return };
    match description {
        Some(description) => eprintln!("{prefix} {name}: {description}"),
        None => eprintln!("{prefix} {name}"),
    }
}

/// How a step list ended: normally, via `break` (caught by the innermost
/// loop), or via `stop` (caught by the running workflow file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    Break,
    Stop,
}

/// The data threaded through a sequential step list: the current value, the
/// progress counter, and the outputs recorded by `id` so far.
#[derive(Debug, Clone, Default)]
pub(crate) struct State {
    pub(crate) value: Value,
    pub(crate) counter: usize,
    pub(crate) steps: StepOutputs,
}

pub(crate) struct Outcome {
    pub(crate) state: State,
    pub(crate) flow: Flow,
}

/// Physical concurrency inherited across workflow-file boundaries, so a
/// child file loaded inside a `parallel` branch or concurrent `for_each`
/// cannot `ask` (or `write` a fixed path concurrently).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Placement {
    #[default]
    Sequential,
    Parallel,
    ConcurrentItems,
}

impl Placement {
    fn enter(self, child: Child) -> Self {
        match child {
            Child::Branch if self != Self::ConcurrentItems => Self::Parallel,
            Child::ConcurrentItem => Self::ConcurrentItems,
            _ => self,
        }
    }
}

/// The read-only environment of a step list: scope, services, concurrency
/// placement, progress prefix, cancellation, and the innermost loop's
/// `{index, item}` context.
#[derive(Clone)]
pub(crate) struct Frame<'a> {
    pub(crate) scope: &'a WorkflowScope,
    pub(crate) env: &'a RunContext,
    pub(crate) placement: Placement,
    pub(crate) prefix: String,
    pub(crate) cancellation: CancellationToken,
    pub(crate) loop_context: Value,
}

impl<'a> Frame<'a> {
    pub(crate) fn new(
        scope: &'a WorkflowScope,
        env: &'a RunContext,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            scope,
            env,
            placement: Placement::Sequential,
            prefix: String::new(),
            cancellation,
            loop_context: Value::Null,
        }
    }

    fn globals(&self, steps: &StepOutputs) -> jq::Globals {
        jq::Globals {
            steps: steps.clone(),
            inputs: self.scope.inputs.clone(),
            loop_context: self.loop_context.clone(),
        }
    }

    fn with_cancellation(&self, cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            ..self.clone()
        }
    }

    fn check_cancelled(&self) -> Result<()> {
        crate::cancellation::check(&self.cancellation, "workflow execution was cancelled")
    }
}

type BoxedOutcome<'a> = Pin<Box<dyn Future<Output = Result<Outcome>> + Send + 'a>>;

/// Runs a step list in order, threading the value, counter, and recorded
/// outputs. Stops early on `break`/`stop`, returning that flow to the caller.
pub(crate) fn run_steps<'a>(steps: &'a [Step], state: State, frame: Frame<'a>) -> BoxedOutcome<'a> {
    Box::pin(async move {
        let mut state = state;
        for step in steps {
            let outcome = run_step(step, state, &frame).await?;
            state = outcome.state;
            if outcome.flow != Flow::Continue {
                return Ok(Outcome {
                    state,
                    flow: outcome.flow,
                });
            }
        }
        Ok(Outcome {
            state,
            flow: Flow::Continue,
        })
    })
}

/// Runs one step: `when`, then the retried/timed-out unit (`input` → kind →
/// `output`), then `on_error` on failure, then records the value under `id`.
async fn run_step(step: &Step, mut state: State, frame: &Frame<'_>) -> Result<Outcome> {
    frame.check_cancelled()?;
    state.counter += 1;
    let label = step.progress_label(state.counter);
    let prefix = &frame.prefix;
    if let Some(when) = &step.when {
        let truthy = jq::eval_bool_async(
            when,
            &state.value,
            &frame.globals(&state.steps),
            frame.cancellation.clone(),
        )
        .await
        .with_context(|| format!("step '{label}': 'when'"))?;
        if !truthy {
            eprintln!("{prefix}[{}] {label} (skipped)", state.counter);
            return Ok(Outcome {
                state,
                flow: Flow::Continue,
            });
        }
    }
    eprintln!("{prefix}[{}] {label}", state.counter);

    let fallback = step
        .on_error
        .as_ref()
        .map(|_| (state.value.clone(), state.steps.clone(), state.counter));
    let mut outcome = match run_with_policy(step, state, frame, &label).await {
        Ok(outcome) => outcome,
        Err(error) => match (&step.on_error, fallback) {
            (Some(handler), Some((incoming, steps, counter))) if !enclosing_cancelled(frame) => {
                eprintln!("{prefix}    -> step failed, running 'on_error': {error:#}");
                let error_value = json!({
                    "error": format!("{error:#}"),
                    "input": incoming,
                });
                run_steps(
                    handler,
                    State {
                        value: error_value,
                        counter,
                        steps,
                    },
                    frame.clone(),
                )
                .await?
            }
            _ => return Err(error),
        },
    };
    if let Some(id) = &step.id {
        outcome
            .state
            .steps
            .insert(id.clone(), outcome.state.value.clone());
    }
    Ok(outcome)
}

/// Whether the enclosing run (not this step's own timeout) was cancelled; a
/// failure caused by that must not be swallowed by `on_error`.
fn enclosing_cancelled(frame: &Frame<'_>) -> bool {
    frame.cancellation.is_cancelled()
}

/// The upper bound on a single wait between retry attempts.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(3600);

/// The `retry` in effect for `step`: its own, else (for an LLM step) the
/// workflow's `default.retry`.
pub(crate) fn effective_retry<'a>(
    step: &'a Step,
    scope: &'a WorkflowScope,
) -> Option<&'a RetryPolicy> {
    step.retry.as_ref().or_else(|| {
        step.calls_model()
            .then_some(scope.defaults.retry.as_ref())
            .flatten()
    })
}

/// The per-attempt `timeout` in effect for `step`, under the same rule as
/// [`effective_retry`].
pub(crate) fn effective_timeout(step: &Step, scope: &WorkflowScope) -> Option<u64> {
    step.timeout.or_else(|| {
        step.calls_model()
            .then_some(scope.defaults.timeout)
            .flatten()
    })
}

async fn run_with_policy(
    step: &Step,
    state: State,
    frame: &Frame<'_>,
    label: &str,
) -> Result<Outcome> {
    let retry = effective_retry(step, frame.scope);
    let timeout = effective_timeout(step, frame.scope);
    let max_attempts = retry.map_or(1, |retry| retry.max_attempts.max(1));
    let backoff = retry.map_or(1.0, |retry| retry.backoff);
    let mut delay =
        Duration::from_secs(retry.map_or(0, |retry| retry.delay_seconds)).min(MAX_RETRY_DELAY);

    let mut attempt = 1usize;
    let mut state = Some(state);
    loop {
        frame.check_cancelled()?;
        let attempt_state = if attempt < max_attempts {
            state.clone().expect("state is kept until the last attempt")
        } else {
            state.take().expect("state is kept until the last attempt")
        };
        tracing::debug!(step = %label, attempt, max_attempts, "step started");
        let result = run_attempt(
            step,
            attempt_state,
            frame,
            label,
            timeout,
            attempt,
            max_attempts,
        )
        .await;
        match result {
            Ok(outcome) => return Ok(outcome),
            Err(error) if attempt < max_attempts => {
                frame.check_cancelled()?;
                eprintln!(
                    "{}    -> attempt {attempt}/{max_attempts} failed: {error}; retrying in {:.1}s",
                    frame.prefix,
                    delay.as_secs_f64()
                );
                wait_retry_delay(delay, &frame.cancellation).await?;
                delay = Duration::try_from_secs_f64((delay.as_secs_f64() * backoff).max(0.0))
                    .unwrap_or(MAX_RETRY_DELAY)
                    .min(MAX_RETRY_DELAY);
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn run_attempt(
    step: &Step,
    state: State,
    frame: &Frame<'_>,
    label: &str,
    timeout: Option<u64>,
    attempt: usize,
    max_attempts: usize,
) -> Result<Outcome> {
    let Some(seconds) = timeout else {
        return run_unit(step, state, frame, label).await;
    };
    // A child token is cancelled by this attempt's own timeout and by the
    // enclosing cancellation alike; the attempt is awaited after cancelling
    // it so a retry never races a still-running process or file write.
    let token = frame.cancellation.child_token();
    let attempt_frame = frame.with_cancellation(token.clone());
    let execution = run_unit(step, state, &attempt_frame, label);
    tokio::pin!(execution);
    match tokio::time::timeout(Duration::from_secs(seconds), &mut execution).await {
        Ok(result) => result,
        Err(_) => {
            token.cancel();
            let _ = execution.await;
            Err(anyhow!(crate::error::Interrupted::timed_out(format!(
                "step '{label}' timed out after {seconds}s (attempt {attempt}/{max_attempts})"
            ))))
        }
    }
}

async fn wait_retry_delay(delay: Duration, cancellation: &CancellationToken) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => bail!(crate::error::Interrupted::cancelled("workflow execution was cancelled")),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

/// One attempt: `input` → the kind → `output`.
async fn run_unit(step: &Step, state: State, frame: &Frame<'_>, label: &str) -> Result<Outcome> {
    let State {
        value,
        counter,
        steps,
    } = state;
    let context = |what: &str| format!("step '{label}': {what}");
    let input = match &step.input {
        Some(expression) => jq::eval_one_async(
            expression,
            &value,
            &frame.globals(&steps),
            frame.cancellation.clone(),
        )
        .await
        .with_context(|| context("'input'"))?,
        None => value,
    };

    let (mut state, flow) = match &step.kind {
        StepKind::Group(body) => {
            let outcome = run_steps(
                body,
                State {
                    value: input,
                    counter,
                    steps,
                },
                frame.clone(),
            )
            .await?;
            (outcome.state, outcome.flow)
        }
        StepKind::Switch(switch) => run_switch(switch, input, counter, steps, frame, label).await?,
        StepKind::Parallel(parallel) => {
            let value = run_parallel(parallel, input, &steps, frame).await?;
            (
                State {
                    value,
                    counter,
                    steps,
                },
                Flow::Continue,
            )
        }
        StepKind::ForEach(for_each) => {
            run_for_each(for_each, input, counter, steps, frame, label).await?
        }
        StepKind::Loop(loop_step) => {
            run_loop(loop_step, input, counter, steps, frame, label).await?
        }
        StepKind::Stop | StepKind::Break => (
            State {
                value: input,
                counter,
                steps,
            },
            if matches!(step.kind, StepKind::Stop) {
                Flow::Stop
            } else {
                Flow::Break
            },
        ),
        action => {
            let globals = frame.globals(&steps);
            let value = run_action(action, input, &globals, frame, label)
                .await
                .with_context(|| format!("step '{label}'"))?;
            (
                State {
                    value,
                    counter,
                    steps,
                },
                Flow::Continue,
            )
        }
    };

    let own_result =
        flow == Flow::Continue || matches!(step.kind, StepKind::Stop | StepKind::Break);
    if own_result && let Some(expression) = &step.output {
        state.value = jq::eval_one_async(
            expression,
            &state.value,
            &frame.globals(&state.steps),
            frame.cancellation.clone(),
        )
        .await
        .with_context(|| context("'output'"))?;
    }
    Ok(Outcome { state, flow })
}

async fn run_switch(
    switch: &SwitchStep,
    input: Value,
    counter: usize,
    steps: StepOutputs,
    frame: &Frame<'_>,
    label: &str,
) -> Result<(State, Flow)> {
    let globals = frame.globals(&steps);
    for (index, case) in switch.cases.iter().enumerate() {
        let matched = jq::eval_bool_async(&case.when, &input, &globals, frame.cancellation.clone())
            .await
            .with_context(|| format!("step '{label}': case {} 'when'", index + 1))?;
        if matched {
            eprintln!("{}    -> case {} matched", frame.prefix, index + 1);
            let outcome = run_steps(
                &case.steps,
                State {
                    value: input,
                    counter,
                    steps,
                },
                frame.clone(),
            )
            .await?;
            return Ok((outcome.state, outcome.flow));
        }
    }
    let Some(else_steps) = &switch.else_steps else {
        bail!("step '{label}': no case matched and no 'else' is defined");
    };
    eprintln!("{}    -> no case matched, running 'else'", frame.prefix);
    let outcome = run_steps(
        else_steps,
        State {
            value: input,
            counter,
            steps,
        },
        frame.clone(),
    )
    .await?;
    Ok((outcome.state, outcome.flow))
}

/// Runs every branch concurrently against the same input, each with its own
/// copy of the recorded outputs (never merged back), and joins their values
/// into an object keyed by branch name, in declaration order.
async fn run_parallel(
    parallel: &ParallelStep,
    input: Value,
    steps: &StepOutputs,
    frame: &Frame<'_>,
) -> Result<Value> {
    eprintln!(
        "{}    -> running {} branches concurrently",
        frame.prefix,
        parallel.branches.len()
    );
    let placement = frame.placement.enter(Child::Branch);
    let futures = parallel.branches.iter().map(|(name, body)| {
        let branch_frame = Frame {
            placement,
            prefix: format!("{}[{name}] ", frame.prefix),
            ..frame.clone()
        };
        run_steps(
            body,
            State {
                value: input.clone(),
                counter: 0,
                steps: steps.clone(),
            },
            branch_frame,
        )
    });
    let results = futures_util::future::try_join_all(futures).await?;
    let mut joined = Map::new();
    for ((name, _), outcome) in parallel.branches.iter().zip(results) {
        joined.insert(name.clone(), outcome.state.value);
    }
    eprintln!("{}    -> branches joined", frame.prefix);
    Ok(Value::Object(joined))
}

async fn run_for_each(
    for_each: &ForEachStep,
    input: Value,
    counter: usize,
    steps: StepOutputs,
    frame: &Frame<'_>,
    label: &str,
) -> Result<(State, Flow)> {
    let items = jq::eval_one_async(
        &for_each.items,
        &input,
        &frame.globals(&steps),
        frame.cancellation.clone(),
    )
    .await
    .with_context(|| format!("step '{label}': 'for_each'"))?;
    let Value::Array(items) = items else {
        bail!(
            "step '{label}': 'for_each' must produce a JSON array, got {}",
            json_type(&items)
        );
    };
    let total = items.len();

    if for_each.max_concurrency > 1 {
        eprintln!(
            "{}    -> iterating over {total} item(s), up to {} concurrently",
            frame.prefix, for_each.max_concurrency
        );
        let placement = frame.placement.enter(Child::ConcurrentItem);
        let futures = items.into_iter().enumerate().map(|(index, item)| {
            let item_frame = Frame {
                placement,
                prefix: format!("{}[item-{}] ", frame.prefix, index + 1),
                loop_context: json!({"index": index, "item": item.clone()}),
                ..frame.clone()
            };
            run_steps(
                &for_each.steps,
                State {
                    value: item,
                    counter: 0,
                    steps: steps.clone(),
                },
                item_frame,
            )
        });
        let results: Vec<Outcome> = futures_util::stream::iter(futures)
            .buffered(for_each.max_concurrency)
            .try_collect()
            .await?;
        let values = results
            .into_iter()
            .map(|outcome| outcome.state.value)
            .collect();
        return Ok((
            State {
                value: Value::Array(values),
                counter,
                steps,
            },
            Flow::Continue,
        ));
    }

    eprintln!("{}    -> iterating over {total} item(s)", frame.prefix);
    let mut results = Vec::with_capacity(total);
    let mut counter = counter;
    let mut steps = steps;
    for (index, item) in items.into_iter().enumerate() {
        eprintln!("{}    -> item {}/{total}", frame.prefix, index + 1);
        let item_frame = Frame {
            loop_context: json!({"index": index, "item": item.clone()}),
            ..frame.clone()
        };
        let outcome = run_steps(
            &for_each.steps,
            State {
                value: item,
                counter,
                steps,
            },
            item_frame,
        )
        .await?;
        counter = outcome.state.counter;
        steps = outcome.state.steps;
        match outcome.flow {
            Flow::Stop => {
                return Ok((
                    State {
                        value: outcome.state.value,
                        counter,
                        steps,
                    },
                    Flow::Stop,
                ));
            }
            Flow::Break => {
                results.push(outcome.state.value);
                break;
            }
            Flow::Continue => results.push(outcome.state.value),
        }
    }
    Ok((
        State {
            value: Value::Array(results),
            counter,
            steps,
        },
        Flow::Continue,
    ))
}

async fn run_loop(
    loop_step: &LoopStep,
    input: Value,
    counter: usize,
    steps: StepOutputs,
    frame: &Frame<'_>,
    label: &str,
) -> Result<(State, Flow)> {
    let keyword = loop_step.condition.keyword();
    let max = loop_step.max_iterations;
    let mut state = State {
        value: input,
        counter,
        steps,
    };
    let mut iteration = 0usize;
    loop {
        if let LoopCondition::While(condition) = &loop_step.condition {
            let holds = jq::eval_bool_async(
                condition,
                &state.value,
                &frame.globals(&state.steps),
                frame.cancellation.clone(),
            )
            .await
            .with_context(|| format!("step '{label}': 'while'"))?;
            if !holds {
                return Ok((state, Flow::Continue));
            }
        }
        if iteration >= max {
            bail!("step '{label}': reached max_iterations ({max}) without satisfying '{keyword}'");
        }
        eprintln!("{}    -> iteration {}/{max}", frame.prefix, iteration + 1);
        let body_frame = Frame {
            loop_context: json!({"index": iteration}),
            ..frame.clone()
        };
        let outcome = run_steps(&loop_step.steps, state, body_frame).await?;
        state = outcome.state;
        iteration += 1;
        match outcome.flow {
            Flow::Stop => return Ok((state, Flow::Stop)),
            Flow::Break => return Ok((state, Flow::Continue)),
            Flow::Continue => {}
        }
        if let LoopCondition::Until(condition) = &loop_step.condition {
            let done = jq::eval_bool_async(
                condition,
                &state.value,
                &frame.globals(&state.steps),
                frame.cancellation.clone(),
            )
            .await
            .with_context(|| format!("step '{label}': 'until'"))?;
            if done {
                return Ok((state, Flow::Continue));
            }
        }
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Runs an action kind against `input`.
async fn run_action(
    kind: &StepKind,
    input: Value,
    globals: &jq::Globals,
    frame: &Frame<'_>,
    label: &str,
) -> Result<Value> {
    let cancellation = frame.cancellation.clone();
    match kind {
        StepKind::Prompt(prompt) => run_prompt(prompt, input, globals, frame, label).await,
        StepKind::Agent(agent) => run_agent_step(agent, input, globals, frame, label).await,
        StepKind::Run(run) => {
            let argv: Vec<String> = run
                .argv
                .iter()
                .map(|arg| template::render_in(arg, &input, globals))
                .collect::<Result<_>>()?;
            // `env`/`cwd` containment (see `config::ShellToolDefinition::env`/
            // `::cwd`) is scoped to `tools:` shell tool definitions only, for
            // now — a workflow `run:` step's other fields never get
            // `${VAR_NAME}` expansion (see AGENTS.md's "Security and
            // Configuration" section), and extending that boundary to a new
            // field needs its own deliberate design pass rather than
            // inheriting shell tools' expansion story by accident.
            let stdout = crate::process::run_command(
                &argv,
                &template::to_text(&input),
                None,
                None,
                cancellation,
            )
            .await?;
            Ok(Value::String(stdout))
        }
        StepKind::Decide(decide) => run_decide(decide, input, frame).await,
        StepKind::Workflow(workflow) => run_child(workflow, input, globals, frame, label).await,
        StepKind::Jq(filter) => jq::eval_one_async(filter, &input, globals, cancellation).await,
        StepKind::Ask(ask) => {
            let prompt = template::render_in(&ask.prompt, &input, globals)?;
            Ok(Value::String(
                super::ask::run_ask(&prompt, ask, cancellation).await?,
            ))
        }
        StepKind::Write(write) => {
            let path = template::render_in(&write.path, &input, globals)?;
            async_io::write_output_file(
                std::path::Path::new(&path),
                &template::to_text(&input),
                cancellation,
            )
            .await?;
            Ok(input)
        }
        StepKind::Group(_)
        | StepKind::Switch(_)
        | StepKind::Parallel(_)
        | StepKind::ForEach(_)
        | StepKind::Loop(_)
        | StepKind::Stop
        | StepKind::Break => bail!(
            "internal error: control step kind '{}' reached run_action; run_unit handles it",
            kind.name()
        ),
    }
}

/// Runs a `decide:` step: the incoming value is the `state`, and the
/// step's result is the `answers` object keyed by question id.
async fn run_decide(step: &DecideStep, input: Value, frame: &Frame<'_>) -> Result<Value> {
    if frame.env.is_replaying() {
        bail!(
            "a 'decide' step cannot run under --replay: cassettes record chat completions only, \
             and Jev requests are not recorded"
        );
    }
    let services = &frame.env.services;
    let endpoint = config::resolve_jev_endpoint(None, None, &services.file_config)?;
    let model = crate::jev::resolve_model(step.model.as_deref(), &services.file_config);
    let decision = crate::jev::decide(
        services,
        &endpoint,
        &model,
        &step.questions,
        crate::jev::state_from_value(input),
        frame.cancellation.clone(),
    )
    .await?;
    Ok(Value::Object(decision.answers))
}

async fn run_prompt(
    step: &PromptStep,
    input: Value,
    globals: &jq::Globals,
    frame: &Frame<'_>,
    label: &str,
) -> Result<Value> {
    let cancellation = frame.cancellation.clone();
    if let Some(input_schema) = &step.input_schema {
        let schema =
            schema::load_schema_value_cancellable(&input_schema.source, cancellation.clone())
                .await?;
        schema::validate_value(&schema, &input, "input")?;
    }
    let settings = resolve_llm_settings(
        &step.llm,
        None,
        frame.scope,
        &frame.env.services.file_config,
        label,
    )?
    .with_usage_label(label);
    let output_schema = match &step.output_schema {
        Some(output_schema) => Some(
            schema::load_schema_value_cancellable(&output_schema.source, cancellation.clone())
                .await?,
        ),
        None => None,
    };
    let response_format = output_schema
        .as_ref()
        .map(|schema| schema::build_json_schema(schema.clone(), step.effective_schema_name()))
        .transpose()?;

    let prompt = template::render_in(&step.prompt, &input, globals)?;
    let system = step
        .system
        .as_deref()
        .or(frame.scope.defaults.system.as_deref())
        .map(|system| template::render_in(system, &input, globals))
        .transpose()?;
    let (prompt, image_urls) = resolve_attachments(
        &step.attachments,
        prompt,
        &input,
        globals,
        cancellation.clone(),
    )
    .await?;

    let response = settings
        .complete(
            frame.env,
            &[],
            PromptTurn {
                system_prompt: system.as_deref(),
                history: &[],
                prompt: &prompt,
                image_urls: &image_urls,
            },
            response_format,
            cancellation,
        )
        .await?;
    let text = response::render_plain(&response)?;
    match output_schema {
        Some(schema) => parse_structured(&text, &schema),
        None => Ok(Value::String(text)),
    }
}

/// An agent definition in hand: inline (from this workflow's `agents:`) or
/// loaded from a file through the shared registry cache.
enum AgentHandle {
    Inline(Arc<AgentFile>),
    Loaded(Arc<subagent::LoadedAgent>),
}

impl AgentHandle {
    fn file(&self) -> &AgentFile {
        match self {
            Self::Inline(definition) => definition,
            Self::Loaded(loaded) => &loaded.file,
        }
    }
}

async fn load_agent(
    reference: &AgentRef,
    frame: &Frame<'_>,
) -> Result<(AgentHandle, Vec<PathBuf>)> {
    let path = match reference {
        AgentRef::Inline { definition, .. } => {
            return Ok((AgentHandle::Inline(Arc::clone(definition)), Vec::new()));
        }
        AgentRef::Path(path) => path.clone(),
        AgentRef::Registry(name) => frame
            .env
            .services
            .file_config
            .agents
            .get(name)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "agent '{name}' is neither defined in this workflow's 'agents:' nor \
                     registered under 'agents:' in {}",
                    config::CONFIG_FILE_NAME
                )
            })?,
    };
    let loaded = frame
        .env
        .services
        .agent_registry
        .load_path_cancellable(&path, frame.cancellation.clone())
        .await?;
    let active = vec![loaded.canonical_path.clone()];
    Ok((AgentHandle::Loaded(loaded), active))
}

async fn run_agent_step(
    step: &AgentStep,
    input: Value,
    globals: &jq::Globals,
    frame: &Frame<'_>,
    label: &str,
) -> Result<Value> {
    let cancellation = frame.cancellation.clone();
    let (handle, active_paths) = load_agent(&step.agent, frame).await?;
    let agent_file = handle.file();
    match &handle {
        AgentHandle::Loaded(loaded) => loaded.validate_input(&input)?,
        AgentHandle::Inline(definition) => {
            if let Some(source) = &definition.input_schema {
                let schema =
                    schema::load_schema_value_cancellable(source, cancellation.clone()).await?;
                schema::validate_value(&schema, &input, "input")?;
            }
        }
    }
    let settings = resolve_llm_settings(
        &step.llm,
        Some(agent_file),
        frame.scope,
        &frame.env.services.file_config,
        label,
    )?
    .with_usage_label(label);
    let (prompt, image_urls) = resolve_attachments(
        &step.attachments,
        template::to_text(&input),
        &input,
        globals,
        cancellation.clone(),
    )
    .await?;
    let text = call_agent(
        agent_file,
        &settings,
        frame.env,
        AgentTurn {
            input: &input,
            prompt: &prompt,
            image_urls: &image_urls,
        },
        globals,
        &active_paths,
        cancellation.clone(),
    )
    .await?;
    match &agent_file.output_schema {
        Some(source) => {
            let schema = schema::load_schema_value_cancellable(source, cancellation).await?;
            parse_structured(&text, &schema)
        }
        None => Ok(Value::String(text)),
    }
}

/// Parses a Structured Outputs response and checks it against its schema.
/// A single surrounding Markdown code fence is tolerated, since some
/// OpenAI-compatible servers add one despite `response_format`.
fn parse_structured(text: &str, schema: &Value) -> Result<Value> {
    let trimmed = text.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let value: Value = serde_json::from_str(unfenced)
        .with_context(|| format!("the structured output was not valid JSON: {text:?}"))?;
    schema::validate_value(schema, &value, "output")?;
    Ok(value)
}

/// Renders and reads a step's `files:`/`images:`: file contents are
/// appended to `prompt` as named fenced blocks; images become `image_url`
/// parts. Paths are relative to the current working directory.
async fn resolve_attachments(
    attachments: &Attachments,
    prompt: String,
    input: &Value,
    globals: &jq::Globals,
    cancellation: CancellationToken,
) -> Result<(String, Vec<String>)> {
    if attachments.files.is_empty() && attachments.images.is_empty() {
        return Ok((prompt, Vec::new()));
    }
    let files: Vec<PathBuf> = attachments
        .files
        .iter()
        .map(|file| template::render_in(file, input, globals).map(PathBuf::from))
        .collect::<Result<_>>()?;
    let images: Vec<String> = attachments
        .images
        .iter()
        .map(|image| template::render_in(image, input, globals))
        .collect::<Result<_>>()?;
    let (file_context, image_urls) = tokio::try_join!(
        attachment::read_file_attachments_cancellable(&files, cancellation.clone()),
        attachment::resolve_image_urls_cancellable(&images, cancellation),
    )?;
    let prompt = match file_context {
        Some(context) => format!("{prompt}\n\n{context}"),
        None => prompt,
    };
    Ok((prompt, image_urls))
}

/// Resolves the request settings of an LLM step: step → agent definition
/// (for an `agent` step) → workflow `default:` → `lait.config.yml`
/// `default:`, each field independently. Also used (read-only) by
/// `lait run --dry-run`.
pub(crate) fn resolve_llm_settings(
    llm: &LlmOverrides,
    agent: Option<&AgentFile>,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    label: &str,
) -> Result<RequestSettings> {
    let model_name = llm
        .model
        .clone()
        .or_else(|| agent.and_then(|agent| agent.model.clone()))
        .or_else(|| scope.defaults.model.clone())
        .or_else(|| file_config.default.model.clone())
        .ok_or_else(|| {
            anyhow!(
                "model is required for step '{label}'; set 'model' on the step,{} the workflow's \
                 default.model, or default.model in {}",
                if agent.is_some() {
                    " its agent definition,"
                } else {
                    ""
                },
                config::CONFIG_FILE_NAME
            )
        })?;
    let step_sampling = SamplingOverrides {
        reasoning_effort: llm.reasoning_effort,
        temperature: llm.temperature,
        top_p: llm.top_p,
        max_tokens: llm.max_tokens,
    };
    let agent_sampling = agent
        .map(|agent| SamplingOverrides {
            reasoning_effort: agent.reasoning_effort,
            temperature: agent.temperature,
            top_p: agent.top_p,
            max_tokens: agent.max_tokens,
        })
        .unwrap_or_default();
    let workflow_sampling = SamplingOverrides {
        reasoning_effort: scope.defaults.reasoning_effort,
        temperature: scope.defaults.temperature,
        top_p: scope.defaults.top_p,
        max_tokens: scope.defaults.max_tokens,
    };
    let step_capability = CapabilityOverrides {
        mcp: llm.mcp.clone(),
        max_tool_rounds: llm.max_tool_rounds,
        skills: llm.skills.clone(),
        subagents: llm.subagents.clone(),
        tools: llm.tools.clone(),
    };
    let agent_capability = agent
        .map(|agent| CapabilityOverrides {
            mcp: agent.mcp.clone(),
            max_tool_rounds: agent.max_tool_rounds,
            skills: agent.skills.clone(),
            subagents: agent.subagents.clone(),
            tools: agent.tools.clone(),
        })
        .unwrap_or_default();
    let workflow_capability = CapabilityOverrides {
        mcp: scope.defaults.mcp.clone(),
        max_tool_rounds: scope.defaults.max_tool_rounds,
        skills: scope.defaults.skills.clone(),
        subagents: scope.defaults.subagents.clone(),
        tools: scope.defaults.tools.clone(),
    };
    resolve_request_settings(
        model_name,
        SamplingOverrides::fold(&[step_sampling, agent_sampling, workflow_sampling]),
        EndpointOverrides::default(),
        CapabilityOverrides::fold([step_capability, agent_capability, workflow_capability]),
        &scope.models,
        file_config,
    )
    .with_context(|| format!("step '{label}'"))
}

async fn run_child(
    step: &WorkflowStep,
    input: Value,
    globals: &jq::Globals,
    frame: &Frame<'_>,
    label: &str,
) -> Result<Value> {
    let cancellation = frame.cancellation.clone();
    let path = match &step.workflow {
        WorkflowRef::Path(path) => path.clone(),
        WorkflowRef::Registry(name) => frame
            .env
            .services
            .file_config
            .workflows
            .get(name)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "workflow '{name}' is not registered under 'workflows:' in {}; use a path \
                     such as './{name}.yml' for a file",
                    config::CONFIG_FILE_NAME
                )
            })?,
    };
    // Resolve cycles before opening the child: a recursive FIFO reference
    // must fail rather than wait for a second writer.
    let canonical = frame
        .scope
        .check_nested_path(&path, label, cancellation.clone())
        .await?;
    let child = frame
        .env
        .services
        .workflow_registry
        .load_path_cancellable(&canonical, cancellation.clone())
        .await?;
    validate_execution_placement(&child.steps, frame.placement)
        .with_context(|| format!("workflow '{}'", canonical.display()))?;
    let provided = match &step.with {
        Some(expression) => {
            match jq::eval_one_async(expression, &input, globals, cancellation.clone())
                .await
                .context("'with'")?
            {
                Value::Object(object) => inputs::from_object(object),
                other => bail!("'with' must produce an object, got {}", json_type(&other)),
            }
        }
        None => Vec::new(),
    };
    let resolved = inputs::resolve(&child.inputs, provided)
        .with_context(|| format!("workflow '{}'", canonical.display()))?;
    let input = inputs::resolve_initial(
        &child,
        inputs::InitialInput::Value(input),
        cancellation.clone(),
    )
    .await
    .with_context(|| format!("workflow '{}'", canonical.display()))?;
    let child_scope = frame.scope.nested(canonical, &child, resolved);
    announce_named_file(
        &format!("{}    ->", frame.prefix),
        child.name.as_deref(),
        child.description.as_deref(),
    );
    let child_frame = Frame {
        scope: &child_scope,
        env: frame.env,
        placement: frame.placement,
        prefix: format!("{}    ", frame.prefix),
        cancellation,
        loop_context: Value::Null,
    };
    run_document(&child, input, child_frame)
        .await
        .map(|(output, _)| output)
}

/// Rejects, before any of a child file's steps run, an `ask` inside
/// concurrent execution or a fixed-path `write` inside concurrent items —
/// restrictions inherited from where the child is being called.
fn validate_execution_placement(steps: &[Step], placement: Placement) -> Result<()> {
    for step in steps {
        let label = step.id.as_deref().unwrap_or(step.kind.name());
        match &step.kind {
            StepKind::Ask(_) if placement != Placement::Sequential => bail!(
                "step '{label}' cannot 'ask' inside concurrent execution, including a nested workflow"
            ),
            StepKind::Write(write)
                if placement == Placement::ConcurrentItems && !write.is_dynamic() =>
            {
                bail!(
                    "step '{label}' writes a fixed path inside a concurrent 'for_each', including a \
                     nested workflow; add a placeholder to the path or move the write after the loop"
                )
            }
            _ => {}
        }
        let mut result = Ok(());
        step.for_each_child(|child, children| {
            if result.is_ok() {
                result = validate_execution_placement(children, placement.enter(child));
            }
        });
        result?;
    }
    Ok(())
}

/// Runs a whole workflow file against `initial`: its steps (a `stop`
/// finishes the file early), then its `output`, all under its `timeout`.
/// Returns the final output alongside the step outputs recorded by `id`, so
/// `lait test`/`lait eval` can check a `step_output` trajectory assertion.
pub(crate) async fn run_document(
    wf: &WorkflowFile,
    initial: Value,
    frame: Frame<'_>,
) -> Result<(Value, StepOutputs)> {
    let Some(seconds) = wf.timeout else {
        return run_document_inner(wf, initial, frame).await;
    };
    let token = frame.cancellation.child_token();
    let frame = frame.with_cancellation(token.clone());
    let execution = run_document_inner(wf, initial, frame);
    tokio::pin!(execution);
    match tokio::time::timeout(Duration::from_secs(seconds), &mut execution).await {
        Ok(result) => result,
        Err(_) => {
            token.cancel();
            let _ = execution.await;
            Err(anyhow!(crate::error::Interrupted::timed_out(format!(
                "workflow timed out after {seconds}s ('timeout')"
            ))))
        }
    }
}

async fn run_document_inner(
    wf: &WorkflowFile,
    initial: Value,
    frame: Frame<'_>,
) -> Result<(Value, StepOutputs)> {
    let scope = frame.scope;
    let cancellation = frame.cancellation.clone();
    let outcome = run_steps(
        &wf.steps,
        State {
            value: initial,
            counter: 0,
            steps: StepOutputs::new(),
        },
        frame,
    )
    .await?;
    let steps = outcome.state.steps.clone();
    let output = finish_output(wf, scope, outcome.state, cancellation).await?;
    Ok((output, steps))
}

/// Applies a workflow's `output` expression to its final state.
pub(crate) async fn finish_output(
    wf: &WorkflowFile,
    scope: &WorkflowScope,
    state: State,
    cancellation: CancellationToken,
) -> Result<Value> {
    let Some(expression) = &wf.output else {
        return Ok(state.value);
    };
    let globals = jq::Globals {
        steps: state.steps,
        inputs: scope.inputs.clone(),
        loop_context: Value::Null,
    };
    jq::eval_one_async(expression, &state.value, &globals, cancellation)
        .await
        .context("the workflow's 'output'")
}

#[cfg(test)]
mod tests {
    use super::{Frame, State, run_steps};
    use crate::{engine::RunContext, workflow::WorkflowScope};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn a_router_condition_observes_the_enclosing_workflow_cancellation() {
        let path = crate::test_support::unique_temp_path("lait-router-cancel", ".yml");
        std::fs::write(
            &path,
            r#"
steps:
  - switch:
      - when: 'reduce range(0; 100000000) as $i (false; .)'
        steps:
          - stop: true
    else:
      - stop: true
"#,
        )
        .expect("router workflow fixture should be writable");
        let workflow = crate::workflow::load_workflow(&path).unwrap();
        let scope = WorkflowScope::top_level(
            &workflow,
            &path,
            serde_json::Map::new(),
            crate::cancellation::none(),
        )
        .await
        .unwrap();
        let config = std::sync::Arc::new(crate::config::ConfigFile::default());
        let env = RunContext::new(
            std::sync::Arc::new(crate::engine::AppServices::new(config)),
            CancellationToken::new(),
        );
        let token = CancellationToken::new();
        let started = std::time::Instant::now();
        let execution = run_steps(
            &workflow.steps,
            State::default(),
            Frame::new(&scope, &env, token.clone()),
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
