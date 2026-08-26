use anyhow::{Result, bail};

use super::model::{
    ForEachDefinition, LoopDefinition, ParallelDefinition, RetryDefinition, Router, StepDefinition,
    SwitchDefinition, WorkflowDefaults,
};

/// A named predicate over a `StepDefinition`, used by `ACTION_FIELDS`.
type ActionField = (&'static str, fn(&StepDefinition) -> bool);

/// The fields that drive a model call or data transform (as opposed to just
/// `id`), each paired with its name for use in an error message. Kept as a
/// single list so `has_action_fields` and `action_fields_desc` can't drift
/// out of sync when a field is added or removed.
const ACTION_FIELDS: &[ActionField] = &[
    ("when", |step| step.when.is_some()),
    ("model", |step| step.model.is_some()),
    ("reasoning_effort", |step| step.reasoning_effort.is_some()),
    ("prompt", |step| step.prompt.is_some()),
    ("agent", |step| step.agent.is_some()),
    ("workflow", |step| step.workflow.is_some()),
    ("input_schema", |step| step.input_schema.is_some()),
    ("output_schema", |step| step.output_schema.is_some()),
    ("schema_name", |step| step.schema_name.is_some()),
    ("jq", |step| step.jq.is_some()),
    ("write_file", |step| step.write_file.is_some()),
    ("retry", |step| step.retry.is_some()),
    ("timeout", |step| step.timeout.is_some()),
    ("on_error", |step| step.on_error.is_some()),
    ("stop", |step| step.stop.is_some()),
    ("break", |step| step.r#break.is_some()),
];

/// Whether `step` has any field that drives a model call or data transform
/// (as opposed to just `id`), used to reject a `switch`/`parallel`/`loop`/
/// `for_each` step that also sets one of these — they route to nested steps
/// instead of acting directly.
fn has_action_fields(step: &StepDefinition) -> bool {
    ACTION_FIELDS.iter().any(|(_, is_set)| is_set(step))
}

/// A human-readable, comma-separated list of `ACTION_FIELDS`' names, quoted
/// and with a trailing "or", for use in the "it cannot also have ..." bails.
fn action_fields_desc() -> String {
    let (last, rest) = ACTION_FIELDS
        .split_last()
        .expect("ACTION_FIELDS must not be empty");
    let quoted_rest: Vec<String> = rest.iter().map(|(name, _)| format!("'{name}'")).collect();
    format!("{}, or '{}'", quoted_rest.join(", "), last.0)
}

/// Rejects `step` if it has any `ACTION_FIELDS` set, since it is about to be
/// validated as a `router_name` (`switch`/`parallel`/`loop`/`for_each`) step,
/// which routes to nested steps instead of acting directly.
fn reject_action_fields_on_router(
    step: &StepDefinition,
    router_name: &str,
    label: &str,
) -> Result<()> {
    if has_action_fields(step) {
        bail!(
            "step '{label}' has '{router_name}' set; it cannot also have {}",
            action_fields_desc()
        );
    }
    Ok(())
}

/// Tracks the lexical nesting `validate_steps` is currently inside, used to
/// validate `break`/`stop`/`write_file` placement. `in_loop` requires an
/// enclosing `loop`/`for_each` body reachable without crossing a `parallel`
/// branch boundary (concurrently running branches can't share a single loop's
/// break target, so entering a branch resets it). `in_parallel_branch` marks
/// any depth inside a `parallel` branch, since there is no well-defined "the
/// workflow" to `stop` while sibling branches may still be running.
/// `in_concurrent_for_each` marks depth inside a `for_each` body whose
/// `max_concurrency` is above 1: unlike a `parallel` branch (a distinct,
/// separately-authored step list per branch), every concurrently running item
/// there executes the exact same step list, so a `write_file` with its static
/// path would race against itself.
#[derive(Clone, Copy)]
pub(super) struct FlowContext {
    in_loop: bool,
    in_parallel_branch: bool,
    in_concurrent_for_each: bool,
}

impl FlowContext {
    pub(super) const TOP_LEVEL: Self = Self {
        in_loop: false,
        in_parallel_branch: false,
        in_concurrent_for_each: false,
    };

    fn in_loop_body(self) -> Self {
        Self {
            in_loop: true,
            ..self
        }
    }

    fn in_parallel_branch(self) -> Self {
        Self {
            in_loop: false,
            in_parallel_branch: true,
            ..self
        }
    }

    fn in_concurrent_for_each_body(self) -> Self {
        Self {
            in_loop: false,
            in_parallel_branch: true,
            in_concurrent_for_each: true,
        }
    }
}

pub(super) fn validate_steps(steps: &[StepDefinition], ctx: FlowContext) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let label = step
            .id
            .clone()
            .unwrap_or_else(|| format!("step-{}", index + 1));

        let router_count = [
            step.switch.is_some(),
            step.parallel.is_some(),
            step.r#loop.is_some(),
            step.for_each.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count();
        if router_count > 1 {
            bail!(
                "step '{}' can have at most one of 'switch', 'parallel', 'loop', or 'for_each'",
                label
            );
        }

        // `router_count` above guarantees at most one of these is set, so
        // `step.router()` (which just checks them in a fixed order) can't
        // silently prefer one over another here.
        match step.router() {
            Some(Router::Switch(switch)) => {
                validate_switch(step, switch, &label, ctx)?;
                continue;
            }
            Some(Router::Parallel(parallel)) => {
                validate_parallel(step, parallel, &label, ctx)?;
                continue;
            }
            Some(Router::Loop(loop_def)) => {
                validate_loop(step, loop_def, &label, ctx)?;
                continue;
            }
            Some(Router::ForEach(for_each)) => {
                validate_for_each(step, for_each, &label, ctx)?;
                continue;
            }
            None => {}
        }

        if let Some(retry) = &step.retry {
            validate_retry(retry, &format!("step '{label}'"))?;
        }
        if let Some(timeout) = step.timeout {
            validate_timeout(timeout, &format!("step '{label}'"))?;
        }
        if step.write_file.is_some() && ctx.in_concurrent_for_each {
            bail!(
                "step '{}' has 'write_file' set inside a 'for_each' body with 'max_concurrency' \
                 above 1; every concurrently running item would write the same static path. \
                 Move it after the 'for_each' step, or set 'max_concurrency: 1'",
                label
            );
        }
        if let Some(on_error) = &step.on_error {
            if on_error.steps.is_empty() {
                bail!("step '{}' has 'on_error' with an empty 'steps' list", label);
            }
            validate_steps(&on_error.steps, ctx)?;
        }

        if step.r#break == Some(true) && step.stop == Some(true) {
            bail!(
                "step '{}' cannot have both 'stop: true' and 'break: true'",
                label
            );
        }
        if step.r#break == Some(true) && !ctx.in_loop {
            bail!(
                "step '{}' has 'break: true' outside a 'loop'/'for_each' body",
                label
            );
        }
        if step.stop == Some(true) && ctx.in_parallel_branch {
            bail!(
                "step '{}' has 'stop: true' inside a 'parallel' branch, where there is no \
                 single well-defined workflow to stop",
                label
            );
        }

        let calls_model = step.prompt.is_some() || step.agent.is_some() || step.workflow.is_some();
        if !calls_model
            && step.jq.is_none()
            && step.write_file.is_none()
            && step.stop.is_none()
            && step.r#break.is_none()
        {
            bail!(
                "step '{}' must have a 'prompt', an 'agent', a 'workflow', a 'jq' filter, a \
                 'write_file' path, a 'switch', a 'parallel', a 'loop', a 'for_each', 'stop', \
                 'break', or a combination",
                label
            );
        }
        if step.prompt.is_some() && step.agent.is_some() {
            bail!("step '{}' cannot have both 'prompt' and 'agent'", label);
        }
        if step.workflow.is_some() && step.prompt.is_some() {
            bail!("step '{}' cannot have both 'prompt' and 'workflow'", label);
        }
        if step.workflow.is_some() && step.agent.is_some() {
            bail!("step '{}' cannot have both 'agent' and 'workflow'", label);
        }
        if step.agent.is_some()
            && (step.input_schema.is_some()
                || step.output_schema.is_some()
                || step.schema_name.is_some())
        {
            bail!(
                "step '{}' has 'agent' set; 'input_schema'/'output_schema'/'schema_name' come from the agent file and must not be set on the step",
                label
            );
        }
        if step.workflow.is_some()
            && (step.model.is_some()
                || step.reasoning_effort.is_some()
                || step.input_schema.is_some()
                || step.output_schema.is_some()
                || step.schema_name.is_some())
        {
            bail!(
                "step '{}' has 'workflow' set; 'model'/'reasoning_effort'/'input_schema'/'output_schema'/'schema_name' come from the referenced workflow file and must not be set on the step",
                label
            );
        }
        if step.workflow.is_some()
            && (step.retry.is_some() || step.timeout.is_some() || step.on_error.is_some())
        {
            bail!(
                "step '{}' has 'workflow' set; 'retry'/'timeout'/'on_error' apply to a single \
                 action and must be set on the steps inside the referenced workflow file instead",
                label
            );
        }
        if !calls_model && step.output_schema.is_some() {
            bail!(
                "step '{}' has 'output_schema' but no 'prompt'/'agent' to apply it to",
                label
            );
        }
        if step.output_schema.is_none() && step.schema_name.is_some() {
            bail!("step '{}' has 'schema_name' but no 'output_schema'", label);
        }
    }
    Ok(())
}

/// Validates a `switch` step: rejects it if it also has any `ACTION_FIELDS`
/// set, requires a non-empty `cases` list (each with non-empty `steps`), and
/// recurses into every case's `steps` (plus `else`'s, if present) with the
/// same `ctx` — a `switch` only ever runs one of its cases, so it doesn't
/// change the loop/parallel nesting its cases validate against.
fn validate_switch(
    step: &StepDefinition,
    switch: &SwitchDefinition,
    label: &str,
    ctx: FlowContext,
) -> Result<()> {
    reject_action_fields_on_router(step, "switch", label)?;
    if switch.cases.is_empty() {
        bail!("step '{}' has 'switch' with an empty 'cases' list", label);
    }
    for case in &switch.cases {
        if case.steps.is_empty() {
            bail!(
                "step '{}' has a 'switch' case with an empty 'steps' list",
                label
            );
        }
        validate_steps(&case.steps, ctx)?;
    }
    if let Some(else_steps) = &switch.else_steps {
        if else_steps.is_empty() {
            bail!("step '{}' has a 'switch' with an empty 'else' list", label);
        }
        validate_steps(else_steps, ctx)?;
    }
    Ok(())
}

/// Validates a `parallel` step: rejects it if it also has any
/// `ACTION_FIELDS` set, requires a non-empty `branches` list (each with
/// non-empty `steps` and a label unique among its siblings), and recurses
/// into every branch's `steps` with `ctx.in_parallel_branch()` — branches run
/// concurrently, so none of them can `stop`/reach an enclosing `loop`'s
/// `break` (see `FlowContext`'s doc comment).
fn validate_parallel(
    step: &StepDefinition,
    parallel: &ParallelDefinition,
    label: &str,
    ctx: FlowContext,
) -> Result<()> {
    reject_action_fields_on_router(step, "parallel", label)?;
    if parallel.branches.is_empty() {
        bail!(
            "step '{}' has 'parallel' with an empty 'branches' list",
            label
        );
    }
    let mut seen_branch_ids = std::collections::HashSet::new();
    for (branch_index, branch) in parallel.branches.iter().enumerate() {
        if branch.steps.is_empty() {
            bail!(
                "step '{}' has a 'parallel' branch with an empty 'steps' list",
                label
            );
        }
        let branch_label = branch.label(branch_index);
        if !seen_branch_ids.insert(branch_label.clone()) {
            bail!(
                "step '{}' has 'parallel' branches with a duplicate id '{}'",
                label,
                branch_label
            );
        }
        validate_steps(&branch.steps, ctx.in_parallel_branch())?;
    }
    Ok(())
}

/// Validates a `loop` step: rejects it if it also has any `ACTION_FIELDS`
/// set, requires exactly one of `while`/`until`, a `max_iterations` of at
/// least 1, and non-empty `steps`, then recurses into `steps` with
/// `ctx.in_loop_body()` so a `break` inside it validates against this loop.
fn validate_loop(
    step: &StepDefinition,
    loop_def: &LoopDefinition,
    label: &str,
    ctx: FlowContext,
) -> Result<()> {
    reject_action_fields_on_router(step, "loop", label)?;
    match (&loop_def.r#while, &loop_def.until) {
        (Some(_), Some(_)) => bail!(
            "step '{}' has 'loop' with both 'while' and 'until'; exactly one is required",
            label
        ),
        (None, None) => bail!(
            "step '{}' has 'loop' with neither 'while' nor 'until'; exactly one is required",
            label
        ),
        _ => {}
    }
    match loop_def.max_iterations {
        None => bail!(
            "step '{}' has 'loop' with no 'max_iterations' (required)",
            label
        ),
        Some(0) => bail!(
            "step '{}' has 'loop' with 'max_iterations: 0'; it must be at least 1",
            label
        ),
        Some(_) => {}
    }
    if loop_def.steps.is_empty() {
        bail!("step '{}' has 'loop' with an empty 'steps' list", label);
    }
    validate_steps(&loop_def.steps, ctx.in_loop_body())?;
    Ok(())
}

/// Validates a `for_each` step: rejects it if it also has any
/// `ACTION_FIELDS` set, requires non-empty `steps` and a `max_concurrency` of
/// at least 1 (when set), then recurses into `steps` with
/// `ctx.in_loop_body()` for a sequential `for_each` (`max_concurrency <= 1`,
/// the default) or `ctx.in_concurrent_for_each_body()` for a concurrent one —
/// see `FlowContext`'s doc comment for why that distinction matters for
/// `break`/`stop`/`write_file` validation.
fn validate_for_each(
    step: &StepDefinition,
    for_each: &ForEachDefinition,
    label: &str,
    ctx: FlowContext,
) -> Result<()> {
    reject_action_fields_on_router(step, "for_each", label)?;
    if for_each.steps.is_empty() {
        bail!("step '{}' has 'for_each' with an empty 'steps' list", label);
    }
    let max_concurrency = match for_each.max_concurrency {
        Some(0) => bail!(
            "step '{}' has 'for_each' with 'max_concurrency: 0'; it must be at least 1",
            label
        ),
        Some(n) => n,
        None => 1,
    };
    let item_ctx = if max_concurrency > 1 {
        ctx.in_concurrent_for_each_body()
    } else {
        ctx.in_loop_body()
    };
    validate_steps(&for_each.steps, item_ctx)?;
    Ok(())
}

/// Validates a `retry` block (a step's own, or the workflow's
/// `default.retry`): `max_attempts` is required and must be at least 1.
/// `description` names what's being validated (e.g. `"step 'foo'"` or
/// `"the workflow's 'default.retry'"`) for the error message.
fn validate_retry(retry: &RetryDefinition, description: &str) -> Result<()> {
    match retry.max_attempts {
        None => bail!("{description} has 'retry' with no 'max_attempts' (required)"),
        Some(0) => bail!("{description} has 'retry' with 'max_attempts: 0'; it must be at least 1"),
        Some(_) => Ok(()),
    }
}

/// Validates a `timeout` value (a step's own, or the workflow's
/// `default.timeout`): must be at least 1 second. `description` is used the
/// same way as in `validate_retry`.
fn validate_timeout(timeout: u64, description: &str) -> Result<()> {
    if timeout == 0 {
        bail!("{description} has 'timeout: 0'; it must be at least 1 second");
    }
    Ok(())
}

/// Validates a workflow file's top-level `default:` block: its `retry`/
/// `timeout`, if set, follow the same rules as a step's own (see
/// `validate_retry`/`validate_timeout`). `model`/`reasoning_effort` need no
/// validation here (any string/`ReasoningEffort` value is acceptable).
pub(super) fn validate_workflow_defaults(defaults: &WorkflowDefaults) -> Result<()> {
    if let Some(retry) = &defaults.retry {
        validate_retry(retry, "the workflow's 'default.retry'")?;
    }
    if let Some(timeout) = defaults.timeout {
        validate_timeout(timeout, "the workflow's 'default.timeout'")?;
    }
    Ok(())
}
