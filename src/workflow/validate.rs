use anyhow::{Result, bail};

use crate::llm::{validate_max_tool_rounds, validate_sampling_params};

use super::model::{
    FlowStep, ForEachDefinition, LoopDefinition, NodeDefinition, NodeMap, ParallelDefinition,
    RetryDefinition, Router, SwitchDefinition, WorkflowDefaults,
};

/// A named router-incompatible field, used by `ROUTER_INCOMPATIBLE_FIELDS`.
/// Unlike the old `ACTION_FIELDS` (removed when `prompt`/`model`/`retry`/etc.
/// moved to `NodeDefinition`, where their exclusivity with `switch`/
/// `parallel`/`loop`/`for_each` is now enforced by the type itself), these
/// four fields still live on `FlowStep` alongside the router fields, so their
/// exclusivity has to be checked here.
type RouterIncompatibleField = (&'static str, fn(&FlowStep) -> bool);

const ROUTER_INCOMPATIBLE_FIELDS: &[RouterIncompatibleField] = &[
    ("when", |step| step.when.is_some()),
    ("on_error", |step| step.on_error.is_some()),
    ("stop", |step| step.stop.is_some()),
    ("break", |step| step.r#break.is_some()),
];

/// Whether `step` has any field a router site (`switch`/`parallel`/`loop`/
/// `for_each`) is not allowed to combine with.
fn has_router_incompatible_fields(step: &FlowStep) -> bool {
    ROUTER_INCOMPATIBLE_FIELDS
        .iter()
        .any(|(_, is_set)| is_set(step))
}

/// A human-readable, comma-separated list of `ROUTER_INCOMPATIBLE_FIELDS`'
/// names, quoted and with a trailing "or", for use in the "it cannot also
/// have ..." bails.
fn router_incompatible_fields_desc() -> String {
    let (last, rest) = ROUTER_INCOMPATIBLE_FIELDS
        .split_last()
        .expect("ROUTER_INCOMPATIBLE_FIELDS must not be empty");
    let quoted_rest: Vec<String> = rest.iter().map(|(name, _)| format!("'{name}'")).collect();
    format!("{}, or '{}'", quoted_rest.join(", "), last.0)
}

/// Rejects `step` if it has any `ROUTER_INCOMPATIBLE_FIELDS` set, since it is
/// about to be validated as a `router_name` (`switch`/`parallel`/`loop`/
/// `for_each`) site, which routes to nested steps instead of acting directly.
fn reject_router_incompatible_fields(
    step: &FlowStep,
    router_name: &str,
    label: &str,
) -> Result<()> {
    if has_router_incompatible_fields(step) {
        bail!(
            "step '{label}' has '{router_name}' set; it cannot also have {}",
            router_incompatible_fields_desc()
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
/// there executes the exact same step list, so a `write_file` node used there
/// would race against itself.
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

pub(super) fn validate_steps(steps: &[FlowStep], nodes: &NodeMap, ctx: FlowContext) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        // Same fallback `run_steps` uses for progress labels/`$steps` keys,
        // so an error here always points at the same name the executor
        // would show for this site.
        let label = step.label_or(index + 1);

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
        if router_count == 1 && step.r#use.is_some() {
            bail!(
                "step '{}' has 'use' set together with a router ('switch'/'parallel'/'loop'/'for_each'); \
                 exactly one is allowed",
                label
            );
        }

        // `router_count` above guarantees at most one of these is set, so
        // `step.router()` (which just checks them in a fixed order) can't
        // silently prefer one over another here.
        match step.router() {
            Some(Router::Switch(switch)) => {
                reject_router_incompatible_fields(step, "switch", &label)?;
                validate_switch(switch, &label, nodes, ctx)?;
                continue;
            }
            Some(Router::Parallel(parallel)) => {
                reject_router_incompatible_fields(step, "parallel", &label)?;
                validate_parallel(parallel, &label, nodes, ctx)?;
                continue;
            }
            Some(Router::Loop(loop_def)) => {
                reject_router_incompatible_fields(step, "loop", &label)?;
                validate_loop(loop_def, &label, nodes, ctx)?;
                continue;
            }
            Some(Router::ForEach(for_each)) => {
                reject_router_incompatible_fields(step, "for_each", &label)?;
                validate_for_each(for_each, &label, nodes, ctx)?;
                continue;
            }
            None => {}
        }

        if step.r#use.is_none() && step.stop.is_none() && step.r#break.is_none() {
            bail!(
                "step '{}' must have a 'use', a 'switch', a 'parallel', a 'loop', a \
                 'for_each', 'stop', 'break', or a combination",
                label
            );
        }
        if step.r#use.is_none() && step.on_error.is_some() {
            bail!(
                "step '{}' has 'on_error' set without 'use'; there is no node action for it \
                 to guard",
                label
            );
        }
        if let Some(node_id) = &step.r#use {
            let Some(node) = nodes.get(node_id) else {
                bail!(
                    "step '{}' has 'use: {}', but no node with that id is defined in 'nodes'",
                    label,
                    node_id
                );
            };
            if let Some(site_id) = &step.id
                && site_id != node_id
                && nodes.contains_key(site_id)
            {
                bail!(
                    "step '{}' has 'id: {}', which collides with a different node of the same id in \
                     'nodes'; '{{{{ steps.{} }}}}'/'$steps.{}' would become ambiguous",
                    label,
                    site_id,
                    site_id,
                    site_id
                );
            }
            if node.write_file.is_some() && ctx.in_concurrent_for_each {
                bail!(
                    "step '{}' uses node '{}', which has 'write_file' set, inside a 'for_each' body \
                     with 'max_concurrency' above 1; every concurrently running item would write the \
                     same static path. Move it after the 'for_each' step, or set 'max_concurrency: 1'",
                    label,
                    node_id
                );
            }
            if let Some(on_error) = &step.on_error {
                if on_error.steps.is_empty() {
                    bail!("step '{}' has 'on_error' with an empty 'steps' list", label);
                }
                validate_steps(&on_error.steps, nodes, ctx)?;
            }
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
    }
    Ok(())
}

/// Validates one `NodeDefinition` in the workflow's `nodes:` map. Unlike
/// `validate_steps`, this runs once per node regardless of how many `use:`
/// sites reference it, and does not depend on `FlowContext` — everything
/// here is a property of the node's own action, not of where it's used.
pub(super) fn validate_node(node: &NodeDefinition, node_id: &str) -> Result<()> {
    let description = format!("node '{node_id}'");

    validate_sampling_params(node.temperature, node.top_p, node.max_tokens, &description)?;
    if let Some(retry) = &node.retry {
        validate_retry(retry, &description)?;
    }
    if let Some(timeout) = node.timeout {
        validate_timeout(timeout, &description)?;
    }
    validate_max_tool_rounds(node.max_tool_rounds, &description)?;

    let calls_model = node.prompt.is_some() || node.agent.is_some() || node.workflow.is_some();
    if !calls_model && node.jq.is_none() && node.write_file.is_none() {
        bail!(
            "{description} must have a 'prompt', an 'agent', a 'workflow', a 'jq' filter, a \
             'write_file' path, or a combination",
        );
    }
    if node.prompt.is_some() && node.agent.is_some() {
        bail!("{description} cannot have both 'prompt' and 'agent'");
    }
    if node.workflow.is_some() && node.prompt.is_some() {
        bail!("{description} cannot have both 'prompt' and 'workflow'");
    }
    if node.workflow.is_some() && node.agent.is_some() {
        bail!("{description} cannot have both 'agent' and 'workflow'");
    }
    if node.agent.is_some()
        && (node.input_schema.is_some()
            || node.output_schema.is_some()
            || node.schema_name.is_some()
            || node.system_prompt.is_some())
    {
        bail!(
            "{description} has 'agent' set; 'input_schema'/'output_schema'/'schema_name'/\
             'system_prompt' come from the agent file and must not be set on the node"
        );
    }
    if node.workflow.is_some()
        && (node.model.is_some()
            || node.reasoning_effort.is_some()
            || node.temperature.is_some()
            || node.top_p.is_some()
            || node.max_tokens.is_some()
            || node.input_schema.is_some()
            || node.output_schema.is_some()
            || node.schema_name.is_some()
            || node.system_prompt.is_some())
    {
        bail!(
            "{description} has 'workflow' set; 'model'/'reasoning_effort'/'temperature'/'top_p'/\
             'max_tokens'/'input_schema'/'output_schema'/'schema_name'/'system_prompt' come from \
             the referenced workflow file and must not be set on the node"
        );
    }
    if node.workflow.is_some() && (node.retry.is_some() || node.timeout.is_some()) {
        bail!(
            "{description} has 'workflow' set; 'retry'/'timeout' apply to a single action and \
             must be set on the steps inside the referenced workflow file instead"
        );
    }
    if node.workflow.is_some()
        && (node.mcp.is_some()
            || node.max_tool_rounds.is_some()
            || node.skills.is_some()
            || node.subagents.is_some())
    {
        bail!(
            "{description} has 'workflow' set; 'mcp'/'max_tool_rounds'/'skills'/'subagents' apply \
             to a single model call and must be set on the steps inside the referenced workflow \
             file instead"
        );
    }
    if !calls_model
        && (node.mcp.is_some()
            || node.max_tool_rounds.is_some()
            || node.skills.is_some()
            || node.subagents.is_some())
    {
        bail!(
            "{description} has 'mcp'/'max_tool_rounds'/'skills'/'subagents' set but no \
             'prompt'/'agent' to apply it to"
        );
    }
    if !calls_model && node.output_schema.is_some() {
        bail!("{description} has 'output_schema' but no 'prompt'/'agent' to apply it to");
    }
    // Reached only once the 'agent'/'workflow' combinations above are ruled
    // out, so this fires exactly for a node with 'system_prompt' but neither
    // 'prompt' nor 'agent' (a jq-only node, or 'workflow' — already rejected
    // above with a more specific message).
    if node.prompt.is_none() && node.system_prompt.is_some() {
        bail!("{description} has 'system_prompt' but no 'prompt' to apply it to");
    }
    if node.output_schema.is_none() && node.schema_name.is_some() {
        bail!("{description} has 'schema_name' but no 'output_schema'");
    }
    Ok(())
}

/// Validates a `switch` site: requires a non-empty `cases` list (each with
/// non-empty `steps`), and recurses into every case's `steps` (plus `else`'s,
/// if present) with the same `ctx` — a `switch` only ever runs one of its
/// cases, so it doesn't change the loop/parallel nesting its cases validate
/// against.
fn validate_switch(
    switch: &SwitchDefinition,
    label: &str,
    nodes: &NodeMap,
    ctx: FlowContext,
) -> Result<()> {
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
        validate_steps(&case.steps, nodes, ctx)?;
    }
    if let Some(else_steps) = &switch.else_steps {
        if else_steps.is_empty() {
            bail!("step '{}' has a 'switch' with an empty 'else' list", label);
        }
        validate_steps(else_steps, nodes, ctx)?;
    }
    Ok(())
}

/// Validates a `parallel` site: requires a non-empty `branches` list (each
/// with non-empty `steps` and a label unique among its siblings), and
/// recurses into every branch's `steps` with `ctx.in_parallel_branch()` —
/// branches run concurrently, so none of them can `stop`/reach an enclosing
/// `loop`'s `break` (see `FlowContext`'s doc comment).
fn validate_parallel(
    parallel: &ParallelDefinition,
    label: &str,
    nodes: &NodeMap,
    ctx: FlowContext,
) -> Result<()> {
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
        validate_steps(&branch.steps, nodes, ctx.in_parallel_branch())?;
    }
    Ok(())
}

/// Validates a `loop` site: requires exactly one of `while`/`until`, a
/// `max_iterations` of at least 1, and non-empty `steps`, then recurses into
/// `steps` with `ctx.in_loop_body()` so a `break` inside it validates against
/// this loop.
fn validate_loop(
    loop_def: &LoopDefinition,
    label: &str,
    nodes: &NodeMap,
    ctx: FlowContext,
) -> Result<()> {
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
    validate_steps(&loop_def.steps, nodes, ctx.in_loop_body())?;
    Ok(())
}

/// Validates a `for_each` site: requires non-empty `steps` and a
/// `max_concurrency` of at least 1 (when set), then recurses into `steps`
/// with `ctx.in_loop_body()` for a sequential `for_each` (`max_concurrency <=
/// 1`, the default) or `ctx.in_concurrent_for_each_body()` for a concurrent
/// one — see `FlowContext`'s doc comment for why that distinction matters for
/// `break`/`stop`/`write_file` validation.
fn validate_for_each(
    for_each: &ForEachDefinition,
    label: &str,
    nodes: &NodeMap,
    ctx: FlowContext,
) -> Result<()> {
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
    validate_steps(&for_each.steps, nodes, item_ctx)?;
    Ok(())
}

/// Validates a `retry` block (a node's own, or the workflow's
/// `default.retry`): `max_attempts` is required and must be at least 1, and
/// `backoff` (when set) must be a finite number of at least 0 — YAML happily
/// parses `.inf`/`.nan`/a negative multiplier, none of which describe a real
/// retry schedule. `description` names what's being validated (e.g. `"node
/// 'foo'"` or `"the workflow's 'default.retry'"`) for the error message.
fn validate_retry(retry: &RetryDefinition, description: &str) -> Result<()> {
    match retry.max_attempts {
        None => bail!("{description} has 'retry' with no 'max_attempts' (required)"),
        Some(0) => bail!("{description} has 'retry' with 'max_attempts: 0'; it must be at least 1"),
        Some(_) => {}
    }
    if let Some(backoff) = retry.backoff
        && !(backoff.is_finite() && backoff >= 0.0)
    {
        bail!(
            "{description} has 'retry' with 'backoff: {backoff}'; it must be a finite number of \
             at least 0"
        );
    }
    Ok(())
}

/// Validates a `timeout` value (a node's own, or the workflow's
/// `default.timeout`): must be at least 1 second. `description` is used the
/// same way as in `validate_retry`.
fn validate_timeout(timeout: u64, description: &str) -> Result<()> {
    if timeout == 0 {
        bail!("{description} has 'timeout: 0'; it must be at least 1 second");
    }
    Ok(())
}

/// Validates a workflow file's top-level `default:` block: its `retry`/
/// `timeout`, if set, follow the same rules as a node's own (see
/// `validate_retry`/`validate_timeout`), and its `temperature`/`top_p`/
/// `max_tokens` follow the same range rules as a node's own (see
/// `validate_sampling_params`). `model`/`reasoning_effort` need no validation
/// here (any string/`ReasoningEffort` value is acceptable).
pub(super) fn validate_workflow_defaults(defaults: &WorkflowDefaults) -> Result<()> {
    validate_sampling_params(
        defaults.temperature,
        defaults.top_p,
        defaults.max_tokens,
        "the workflow's 'default'",
    )?;
    if let Some(retry) = &defaults.retry {
        validate_retry(retry, "the workflow's 'default.retry'")?;
    }
    if let Some(timeout) = defaults.timeout {
        validate_timeout(timeout, "the workflow's 'default.timeout'")?;
    }
    validate_max_tool_rounds(
        defaults.max_tool_rounds,
        "the workflow's 'default.max_tool_rounds'",
    )?;
    Ok(())
}
