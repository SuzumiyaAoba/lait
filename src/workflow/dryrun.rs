//! `lait run --dry-run`: prints a workflow's execution plan (step order,
//! resolved model/base_url, effective `retry`/`timeout`, and the
//! `when`/`switch`/`parallel`/`loop`/`for_each` control-flow structure)
//! without ever calling a model, spawning an MCP server, or running a
//! `command` node — see docs/usage/ja/workflow.md. `print_plan` walks the
//! step tree once (`build_steps` and its helpers below), doing whatever
//! fallible I/O a real run's own settings resolution would (only reading an
//! `agent:` node's own Markdown file — needed to resolve its model the same
//! way a real run would — never a network/process call), into a
//! [`DryRunPlan`]; [`render_plan`] then turns that into the final printed
//! text. Splitting it this way — mirroring `graph.rs`'s `GraphModel`/
//! `render_mermaid` — means `render_plan` is pure and infallible, so a unit
//! test can check the exact display format without a real workflow/agent
//! file or model to resolve. One consequence: a walk that fails partway
//! through (an unloadable agent file, an unresolvable model) now prints
//! nothing at all, rather than the old `println!`-as-you-go version's
//! truncated plan followed by the error.

use anyhow::{Context, Result};

use crate::{agent, config::ConfigFile, template};

use super::{
    FlowStep, NodeDefinition, Router, WorkflowFile, WorkflowScope,
    exec::{effective_retry, effective_timeout, resolve_step_settings},
};

/// The result of walking a workflow's step tree for `--dry-run`: an ordered
/// list of already-indented display lines. Because the whole plan is a
/// single flat text log (not a graph needing further layout choices, unlike
/// `graph.rs`'s `GraphModel`), "the data" here is simply that line list.
struct DryRunPlan {
    lines: Vec<String>,
}

impl DryRunPlan {
    fn new() -> Self {
        Self { lines: Vec::new() }
    }

    fn push(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }
}

/// Turns `plan`'s lines into the final printed text — pure and infallible,
/// so a unit test can check the exact display format without needing a real
/// workflow file, agent file, or model to resolve (see [`DryRunPlan`]'s doc
/// comment).
fn render_plan(plan: &DryRunPlan) -> String {
    plan.lines.join("\n")
}

/// Prints `wf`'s execution plan to stdout. `initial_prompt`/`vars` are the
/// same `<PROMPT>`/`--var` values a real run would use — a node's own
/// template is rendered against them when possible, so a template that only
/// references `{{ input }}`/`{{ vars.* }}` (the common case for a workflow's
/// first step) shows its real, final text; one that also references
/// `{{ steps.<id> }}` cannot be rendered here (no step has run yet), so its
/// raw template text is shown instead, noted as such.
pub(crate) fn print_plan(
    wf: &WorkflowFile,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    initial_prompt: &str,
    vars: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let mut plan = DryRunPlan::new();
    plan.push(
        "dry run: showing the execution plan only; no model, MCP server, or command process will be invoked",
    );
    let initial_input = template::parse_input(initial_prompt);
    let ctx = DryRunContext {
        scope,
        file_config,
        initial_input: &initial_input,
        vars,
    };
    build_steps(&wf.steps, &ctx, "", &mut plan)?;
    println!("{}", render_plan(&plan));
    Ok(())
}

/// Bundled read-only context threaded through the whole recursive
/// step-tree walk below (`build_steps`/`build_step`/`build_router`/
/// `build_node`) — every field is invariant across the walk; only `indent`
/// (kept as each function's own parameter) changes per recursion depth.
struct DryRunContext<'a> {
    scope: &'a WorkflowScope,
    file_config: &'a ConfigFile,
    initial_input: &'a serde_json::Value,
    vars: &'a serde_json::Map<String, serde_json::Value>,
}

fn build_steps(
    steps: &[FlowStep],
    ctx: &DryRunContext,
    indent: &str,
    plan: &mut DryRunPlan,
) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let label = step.label_or(index + 1);
        build_step(step, &label, index + 1, ctx, indent, plan)?;
    }
    Ok(())
}

fn build_step(
    step: &FlowStep,
    label: &str,
    counter: usize,
    ctx: &DryRunContext,
    indent: &str,
    plan: &mut DryRunPlan,
) -> Result<()> {
    match step.when() {
        Some(when) => plan.push(format!("{indent}[{counter}] {label}  (when: {when})")),
        None => plan.push(format!("{indent}[{counter}] {label}")),
    }

    if let Some(router) = step.router() {
        build_router(router, ctx, indent, plan)?;
        return Ok(());
    }

    if let Some(call) = step.call() {
        build_node(call.definition, call.name, label, ctx, indent, plan)?;
        if let Some(on_error) = step.on_error() {
            plan.push(format!("{indent}    -> on_error:"));
            build_steps(&on_error.steps, ctx, &format!("{indent}       "), plan)?;
        }
    }

    if step.control() == crate::workflow::Control::Stop {
        plan.push(format!(
            "{indent}    -> stop: ends the workflow with this step's output"
        ));
    }
    if step.control() == crate::workflow::Control::Break {
        plan.push(format!(
            "{indent}    -> break: ends the nearest enclosing loop/for_each with this step's output"
        ));
    }

    Ok(())
}

fn build_router(
    router: Router<'_>,
    ctx: &DryRunContext,
    indent: &str,
    plan: &mut DryRunPlan,
) -> Result<()> {
    let inner = format!("{indent}    ");
    let body_indent = format!("{inner}    ");
    match router {
        Router::Switch(switch) => {
            for (index, case) in switch.cases.iter().enumerate() {
                let case_label = case
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("case-{}", index + 1));
                plan.push(format!("{inner}case '{case_label}': when {}", case.when));
                build_steps(&case.steps, ctx, &body_indent, plan)?;
            }
            match &switch.else_steps {
                Some(else_steps) => {
                    plan.push(format!("{inner}else:"));
                    build_steps(else_steps, ctx, &body_indent, plan)?;
                }
                None => plan.push(format!(
                    "{inner}else: (none; no matching case is a runtime error)"
                )),
            }
        }
        Router::Parallel(parallel) => {
            for (index, branch) in parallel.branches.iter().enumerate() {
                plan.push(format!("{inner}branch '{}':", branch.label(index)));
                build_steps(&branch.steps, ctx, &body_indent, plan)?;
            }
            match &parallel.join {
                Some(filter) => plan.push(format!("{inner}join: {filter}")),
                None => plan.push(format!(
                    "{inner}join: (none; branch outputs are joined into an id-keyed object)"
                )),
            }
        }
        Router::Loop(loop_def) => {
            let condition = format!(
                "{} {}",
                loop_def.condition.keyword(),
                loop_def.condition.filter()
            );
            let max_iterations = loop_def.max_iterations;
            plan.push(format!(
                "{inner}{condition}, max_iterations: {max_iterations}"
            ));
            build_steps(&loop_def.steps, ctx, &body_indent, plan)?;
        }
        Router::ForEach(for_each) => {
            plan.push(format!("{inner}items: {}", for_each.items));
            if let Some(max_concurrency) = for_each.max_concurrency {
                plan.push(format!("{inner}max_concurrency: {max_concurrency}"));
            }
            build_steps(&for_each.steps, ctx, &body_indent, plan)?;
            match &for_each.join {
                Some(filter) => plan.push(format!("{inner}join: {filter}")),
                None => plan.push(format!(
                    "{inner}join: (none; per-item outputs are joined into an array)"
                )),
            }
        }
    }
    Ok(())
}

fn build_node(
    node: &NodeDefinition,
    node_id: &str,
    label: &str,
    ctx: &DryRunContext,
    indent: &str,
    plan: &mut DryRunPlan,
) -> Result<()> {
    let inner = format!("{indent}    ");
    plan.push(format!(
        "{inner}use: {node_id}  (type: {})",
        node.type_name()
    ));

    match node {
        NodeDefinition::Prompt(prompt_node) => {
            if let Some(template_text) = &prompt_node.prompt {
                plan.push(format!(
                    "{inner}prompt: {}",
                    render_preview(template_text, ctx.initial_input, ctx.vars)
                ));
            }
            if let Some(template_text) = &prompt_node.system_prompt {
                plan.push(format!(
                    "{inner}system_prompt: {}",
                    render_preview(template_text, ctx.initial_input, ctx.vars)
                ));
            }
            build_model_resolution(node, ctx, None, label, &inner, plan)?;
        }
        NodeDefinition::Agent(agent_node) => {
            plan.push(format!("{inner}agent: {}", agent_node.agent.display()));
            let agent_file = agent::load_agent(&agent_node.agent).with_context(|| {
                format!("step '{label}': failed to load agent file for dry-run")
            })?;
            build_model_resolution(node, ctx, Some(&agent_file), label, &inner, plan)?;
        }
        NodeDefinition::Workflow(workflow_node) => {
            plan.push(format!(
                "{inner}workflow: {} (not expanded; run 'lait run --dry-run' on it directly to inspect it)",
                workflow_node.workflow.display()
            ));
        }
        NodeDefinition::Command(command_node) => {
            let rendered: Vec<String> = command_node
                .command
                .iter()
                .map(|arg| render_preview(arg, ctx.initial_input, ctx.vars))
                .collect();
            plan.push(format!("{inner}command: {}", rendered.join(" ")));
        }
        NodeDefinition::Transform(_) => {}
        NodeDefinition::Ask(ask_node) => {
            plan.push(format!(
                "{inner}prompt: {}",
                render_preview(&ask_node.prompt, ctx.initial_input, ctx.vars)
            ));
            if let Some(choices) = &ask_node.choices {
                plan.push(format!("{inner}choices: {}", choices.join(", ")));
            }
            if ask_node.multiline == Some(true) {
                plan.push(format!("{inner}multiline: true"));
            }
            match &ask_node.default {
                Some(default) => plan.push(format!(
                    "{inner}default: {default}  (used when stdin is not an interactive terminal)"
                )),
                None => plan.push(format!(
                    "{inner}default: none  (fails when stdin is not an interactive terminal)"
                )),
            }
        }
    }

    let settings = node.settings();
    if let Some(filter) = settings.jq {
        plan.push(format!("{inner}jq: {filter}"));
    }
    if let Some(path) = settings.write_file {
        plan.push(format!("{inner}write_file: {}", path.display()));
    }

    match effective_retry(node, ctx.scope) {
        Some(retry) => plan.push(format!(
            "{inner}retry: max_attempts={}, delay_seconds={}, backoff={}",
            retry.max_attempts.unwrap_or(1),
            retry.delay_seconds.unwrap_or(0),
            retry.backoff.unwrap_or(1.0)
        )),
        None => plan.push(format!("{inner}retry: none")),
    }
    match effective_timeout(node, ctx.scope) {
        Some(seconds) => plan.push(format!("{inner}timeout: {seconds}s")),
        None => plan.push(format!("{inner}timeout: none")),
    }

    Ok(())
}

/// Resolves and appends a model-calling node's model/base_url and any
/// `mcp`/`skills`/`subagents` it may use, via the same
/// `exec::resolve_step_settings` a real run calls — but never actually
/// completes a request against it.
fn build_model_resolution(
    node: &NodeDefinition,
    ctx: &DryRunContext,
    agent_file: Option<&agent::AgentFile>,
    label: &str,
    inner: &str,
    plan: &mut DryRunPlan,
) -> Result<()> {
    let settings = resolve_step_settings(node, ctx.scope, ctx.file_config, agent_file, label)?;
    plan.push(format!(
        "{inner}model: {} @ {}",
        settings.resolved_model.model_id, settings.base_url
    ));
    if !settings.mcp.is_empty() {
        plan.push(format!("{inner}mcp: {}", settings.mcp.join(", ")));
    }
    if !settings.skills.is_empty() {
        plan.push(format!("{inner}skills: {}", settings.skills.join(", ")));
    }
    if !settings.subagents.is_empty() {
        plan.push(format!(
            "{inner}subagents: {}",
            settings.subagents.join(", ")
        ));
    }
    Ok(())
}

/// Renders `template_text` against the workflow's initial input/vars when
/// possible, falling back to the raw template text (noted as such) when it
/// references something dry-run has no value for yet (typically
/// `{{ steps.<id> }}`, or a bare `{{ input }}` once a later step's input is
/// no longer the initial one) — see `print_plan`'s doc comment.
fn render_preview(
    template_text: &str,
    initial_input: &serde_json::Value,
    vars: &serde_json::Map<String, serde_json::Value>,
) -> String {
    match template::render(template_text, initial_input, &serde_json::Map::new(), vars) {
        Ok(rendered) => rendered,
        // Falling back to the raw text (rather than propagating the error)
        // is deliberate: dry-run has no value yet for `{{ steps.<id> }}` or
        // a later step's `{{ input }}`, so a render failure here is the
        // expected case, not necessarily a bug. But `error` is still worth
        // showing — it's the only way to tell that expected case apart from
        // an actual template syntax error, which this fallback would
        // otherwise hide until the workflow actually runs.
        Err(error) => format!(
            "{template_text}  [unrendered: depends on the initial input's exact shape or an \
             earlier step's output ({error:#})]"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{DryRunPlan, render_plan};

    #[test]
    fn render_plan_joins_lines_with_newlines() {
        let mut plan = DryRunPlan::new();
        plan.push("dry run: showing the execution plan only");
        plan.push("[1] extract");
        plan.push("    use: extract  (type: prompt)");

        assert_eq!(
            render_plan(&plan),
            "dry run: showing the execution plan only\n\
             [1] extract\n\
             \x20\x20\x20\x20use: extract  (type: prompt)"
        );
    }

    #[test]
    fn render_plan_is_empty_for_an_empty_plan() {
        assert_eq!(render_plan(&DryRunPlan::new()), "");
    }
}
