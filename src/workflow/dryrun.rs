//! `lait run --dry-run`: prints a workflow's execution plan (step order,
//! resolved model/base_url, effective `retry`/`timeout`, and the
//! `when`/`switch`/`parallel`/`loop`/`for_each` control-flow structure)
//! without ever calling a model, spawning an MCP server, or running a
//! `command` node — see docs/usage/ja/workflow.md. `print_plan` and every
//! helper below are synchronous and make no network/process calls; the only
//! I/O is reading an `agent:` node's own Markdown file (needed to resolve its
//! model the same way a real run would).

use anyhow::{Context, Result};

use crate::{agent, config::ConfigFile, template};

use super::{
    FlowStep, NodeDefinition, Router, WorkflowFile, WorkflowScope,
    exec::{effective_retry, effective_timeout, resolve_step_settings},
};

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
    println!(
        "dry run: showing the execution plan only; no model, MCP server, or command process will be invoked"
    );
    let initial_input = template::parse_input(initial_prompt);
    let ctx = DryRunContext {
        scope,
        file_config,
        initial_input: &initial_input,
        vars,
    };
    print_steps(&wf.steps, &ctx, "")
}

/// Bundled read-only context threaded through the whole recursive
/// step-tree walk below (`print_steps`/`print_step`/`print_router`/
/// `print_node`) — every field is invariant across the walk; only `indent`
/// (kept as each function's own parameter) changes per recursion depth.
struct DryRunContext<'a> {
    scope: &'a WorkflowScope,
    file_config: &'a ConfigFile,
    initial_input: &'a serde_json::Value,
    vars: &'a serde_json::Map<String, serde_json::Value>,
}

fn print_steps(steps: &[FlowStep], ctx: &DryRunContext, indent: &str) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let label = step.label_or(index + 1);
        print_step(step, &label, index + 1, ctx, indent)?;
    }
    Ok(())
}

fn print_step(
    step: &FlowStep,
    label: &str,
    counter: usize,
    ctx: &DryRunContext,
    indent: &str,
) -> Result<()> {
    match &step.when {
        Some(when) => println!("{indent}[{counter}] {label}  (when: {when})"),
        None => println!("{indent}[{counter}] {label}"),
    }

    if let Some(router) = step.router() {
        print_router(router, ctx, indent)?;
        return Ok(());
    }

    if let Some(node_id) = &step.r#use {
        // Guaranteed by `validate::validate_steps` before a workflow ever
        // reaches this point (or `execute_step`'s runtime lookup, which the
        // same comment justifies).
        let node = ctx
            .scope
            .nodes
            .get(node_id)
            .expect("validate_steps guarantees 'use' resolves in 'nodes'");
        print_node(node, node_id, label, ctx, indent)?;
        if let Some(on_error) = &step.on_error {
            println!("{indent}    -> on_error:");
            print_steps(&on_error.steps, ctx, &format!("{indent}       "))?;
        }
    }

    if step.stop == Some(true) {
        println!("{indent}    -> stop: ends the workflow with this step's output");
    }
    if step.r#break == Some(true) {
        println!(
            "{indent}    -> break: ends the nearest enclosing loop/for_each with this step's output"
        );
    }

    Ok(())
}

fn print_router(router: Router<'_>, ctx: &DryRunContext, indent: &str) -> Result<()> {
    let inner = format!("{indent}    ");
    let body_indent = format!("{inner}    ");
    match router {
        Router::Switch(switch) => {
            for (index, case) in switch.cases.iter().enumerate() {
                let case_label = case
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("case-{}", index + 1));
                println!("{inner}case '{case_label}': when {}", case.when);
                print_steps(&case.steps, ctx, &body_indent)?;
            }
            match &switch.else_steps {
                Some(else_steps) => {
                    println!("{inner}else:");
                    print_steps(else_steps, ctx, &body_indent)?;
                }
                None => println!("{inner}else: (none; no matching case is a runtime error)"),
            }
        }
        Router::Parallel(parallel) => {
            for (index, branch) in parallel.branches.iter().enumerate() {
                println!("{inner}branch '{}':", branch.label(index));
                print_steps(&branch.steps, ctx, &body_indent)?;
            }
            match &parallel.join {
                Some(filter) => println!("{inner}join: {filter}"),
                None => {
                    println!(
                        "{inner}join: (none; branch outputs are joined into an id-keyed object)"
                    )
                }
            }
        }
        Router::Loop(loop_def) => {
            let condition = match (&loop_def.r#while, &loop_def.until) {
                (Some(cond), _) => format!("while {cond}"),
                (None, Some(cond)) => format!("until {cond}"),
                (None, None) => "(no condition)".to_owned(),
            };
            let max_iterations = loop_def
                .max_iterations
                .map_or_else(|| "?".to_owned(), |n| n.to_string());
            println!("{inner}{condition}, max_iterations: {max_iterations}");
            print_steps(&loop_def.steps, ctx, &body_indent)?;
        }
        Router::ForEach(for_each) => {
            println!("{inner}items: {}", for_each.items);
            if let Some(max_concurrency) = for_each.max_concurrency {
                println!("{inner}max_concurrency: {max_concurrency}");
            }
            print_steps(&for_each.steps, ctx, &body_indent)?;
            match &for_each.join {
                Some(filter) => println!("{inner}join: {filter}"),
                None => println!("{inner}join: (none; per-item outputs are joined into an array)"),
            }
        }
    }
    Ok(())
}

fn print_node(
    node: &NodeDefinition,
    node_id: &str,
    label: &str,
    ctx: &DryRunContext,
    indent: &str,
) -> Result<()> {
    let inner = format!("{indent}    ");
    println!("{inner}use: {node_id}  (type: {})", node.type_name());

    match node {
        NodeDefinition::Prompt(prompt_node) => {
            if let Some(template_text) = &prompt_node.prompt {
                println!(
                    "{inner}prompt: {}",
                    render_preview(template_text, ctx.initial_input, ctx.vars)
                );
            }
            if let Some(template_text) = &prompt_node.system_prompt {
                println!(
                    "{inner}system_prompt: {}",
                    render_preview(template_text, ctx.initial_input, ctx.vars)
                );
            }
            print_model_resolution(node, ctx, None, label, &inner)?;
        }
        NodeDefinition::Agent(agent_node) => {
            println!("{inner}agent: {}", agent_node.agent.display());
            let agent_file = agent::load_agent(&agent_node.agent).with_context(|| {
                format!("step '{label}': failed to load agent file for dry-run")
            })?;
            print_model_resolution(node, ctx, Some(&agent_file), label, &inner)?;
        }
        NodeDefinition::Workflow(workflow_node) => {
            println!(
                "{inner}workflow: {} (not expanded; run 'lait run --dry-run' on it directly to inspect it)",
                workflow_node.workflow.display()
            );
        }
        NodeDefinition::Command(command_node) => {
            let rendered: Vec<String> = command_node
                .command
                .iter()
                .map(|arg| render_preview(arg, ctx.initial_input, ctx.vars))
                .collect();
            println!("{inner}command: {}", rendered.join(" "));
        }
        NodeDefinition::Transform(_) => {}
        NodeDefinition::Ask(ask_node) => {
            println!(
                "{inner}prompt: {}",
                render_preview(&ask_node.prompt, ctx.initial_input, ctx.vars)
            );
            if let Some(choices) = &ask_node.choices {
                println!("{inner}choices: {}", choices.join(", "));
            }
            if ask_node.multiline == Some(true) {
                println!("{inner}multiline: true");
            }
            match &ask_node.default {
                Some(default) => println!(
                    "{inner}default: {default}  (used when stdin is not an interactive terminal)"
                ),
                None => println!(
                    "{inner}default: none  (fails when stdin is not an interactive terminal)"
                ),
            }
        }
    }

    let settings = node.settings();
    if let Some(filter) = settings.jq {
        println!("{inner}jq: {filter}");
    }
    if let Some(path) = settings.write_file {
        println!("{inner}write_file: {}", path.display());
    }

    match effective_retry(node, ctx.scope) {
        Some(retry) => println!(
            "{inner}retry: max_attempts={}, delay_seconds={}, backoff={}",
            retry.max_attempts.unwrap_or(1),
            retry.delay_seconds.unwrap_or(0),
            retry.backoff.unwrap_or(1.0)
        ),
        None => println!("{inner}retry: none"),
    }
    match effective_timeout(node, ctx.scope) {
        Some(seconds) => println!("{inner}timeout: {seconds}s"),
        None => println!("{inner}timeout: none"),
    }

    Ok(())
}

/// Resolves and prints a model-calling node's model/base_url and any
/// `mcp`/`skills`/`subagents` it may use, via the same
/// `exec::resolve_step_settings` a real run calls — but never actually
/// completes a request against it.
fn print_model_resolution(
    node: &NodeDefinition,
    ctx: &DryRunContext,
    agent_file: Option<&agent::AgentFile>,
    label: &str,
    inner: &str,
) -> Result<()> {
    let settings = resolve_step_settings(node, ctx.scope, ctx.file_config, agent_file, label)?;
    println!(
        "{inner}model: {} @ {}",
        settings.resolved_model.model_id, settings.base_url
    );
    if !settings.mcp.is_empty() {
        println!("{inner}mcp: {}", settings.mcp.join(", "));
    }
    if !settings.skills.is_empty() {
        println!("{inner}skills: {}", settings.skills.join(", "));
    }
    if !settings.subagents.is_empty() {
        println!("{inner}subagents: {}", settings.subagents.join(", "));
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
        Err(_) => format!(
            "{template_text}  [unrendered: depends on the initial input's exact shape or an earlier step's output]"
        ),
    }
}
