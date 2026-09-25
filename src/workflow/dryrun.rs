//! `lait run --dry-run`: prints a workflow's execution plan (step order,
//! resolved model/base_url, effective `retry`/`timeout`, and the control-flow
//! structure) without calling a model, spawning an MCP server, or running a
//! command. The only I/O is reading an `agent:` step's agent file, needed to
//! resolve its model the same way a real run would.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::{agent, config::ConfigFile, template};

use super::{
    WorkflowFile, WorkflowScope,
    exec::{effective_retry, effective_timeout, resolve_llm_settings},
    model::{AgentRef, LlmOverrides, Step, StepKind},
};

/// Prints `wf`'s plan. Templates are rendered where their data is already
/// known — `{{ inputs.* }}` always, `{{ input }}` only for the first step of
/// the workflow (whose input is the initial value); anything depending on a
/// step that has not run is shown unrendered.
pub(crate) fn print_plan(
    wf: &WorkflowFile,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    initial_input: &Value,
) -> Result<()> {
    println!(
        "dry run: showing the execution plan only; no model, MCP server, or command process will be invoked"
    );
    if let Some(input_schema) = &wf.input_schema {
        println!("input_schema: {}", input_schema.describe());
    }
    if !scope.inputs.is_empty() {
        println!(
            "inputs: {}",
            serde_json::to_string(&scope.inputs).unwrap_or_default()
        );
    }
    let ctx = DryRunContext { scope, file_config };
    print_steps(&wf.steps, &ctx, "", Some(initial_input))?;
    if let Some(output) = &wf.output {
        println!("output: {output}");
    }
    if let Some(timeout) = wf.timeout {
        println!("timeout: {timeout}s (whole workflow)");
    }
    Ok(())
}

struct DryRunContext<'a> {
    scope: &'a WorkflowScope,
    file_config: &'a ConfigFile,
}

fn print_steps(
    steps: &[Step],
    ctx: &DryRunContext,
    indent: &str,
    first_input: Option<&Value>,
) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let known_input = if index == 0 && step.input.is_none() {
            first_input
        } else {
            None
        };
        print_step(step, index + 1, ctx, indent, known_input)?;
    }
    Ok(())
}

fn print_step(
    step: &Step,
    position: usize,
    ctx: &DryRunContext,
    indent: &str,
    known_input: Option<&Value>,
) -> Result<()> {
    let label = step.label_or(position);
    println!("{indent}[{position}] {label}  ({})", step.kind.name());
    let inner = format!("{indent}    ");
    let body = format!("{inner}    ");
    if let Some(when) = &step.when {
        println!("{inner}when: {when}");
    }
    if let Some(input) = &step.input {
        println!("{inner}input: {input}");
    }
    let preview = |source: &str| render_preview(source, known_input, ctx);

    match &step.kind {
        StepKind::Prompt(prompt) => {
            println!("{inner}prompt: {}", preview(&prompt.prompt));
            if let Some(system) = prompt
                .system
                .as_deref()
                .or(ctx.scope.defaults.system.as_deref())
            {
                println!("{inner}system: {}", preview(system));
            }
            if let Some(schema) = &prompt.input_schema {
                println!("{inner}input_schema: {}", schema.describe());
            }
            if let Some(schema) = &prompt.output_schema {
                println!(
                    "{inner}output_schema: {} (name: {})",
                    schema.describe(),
                    prompt.effective_schema_name()
                );
            }
            print_model(&prompt.llm, None, ctx, &label, &inner)?;
        }
        StepKind::Agent(step_agent) => {
            println!("{inner}agent: {}", step_agent.agent.describe());
            let loaded;
            let definition = match &step_agent.agent {
                AgentRef::Inline { definition, .. } => Some(definition.as_ref()),
                AgentRef::Path(path) => {
                    loaded = agent::load_agent(path).with_context(|| {
                        format!("step '{label}': failed to load agent file for dry-run")
                    })?;
                    Some(&loaded)
                }
                AgentRef::Registry(name) => match ctx.file_config.agents.get(name) {
                    Some(path) => {
                        loaded = agent::load_agent(path).with_context(|| {
                            format!("step '{label}': failed to load agent '{name}' for dry-run")
                        })?;
                        Some(&loaded)
                    }
                    None => {
                        println!("{inner}(agent '{name}' is not registered; the run would fail)");
                        None
                    }
                },
            };
            if let Some(definition) = definition {
                print_model(&step_agent.llm, Some(definition), ctx, &label, &inner)?;
            }
        }
        StepKind::Run(run) => {
            let rendered: Vec<String> = run.argv.iter().map(|arg| preview(arg)).collect();
            println!("{inner}run: {}", rendered.join(" "));
            if !run.env.is_empty() {
                let mut names: Vec<&str> = run.env.keys().map(String::as_str).collect();
                names.sort_unstable();
                println!("{inner}env (allowlist): {}", names.join(", "));
            }
            if let Some(cwd) = &run.cwd {
                println!("{inner}cwd: {cwd}");
            }
        }
        StepKind::Workflow(workflow) => {
            println!(
                "{inner}workflow: {} (not expanded; run 'lait run --dry-run' on it directly)",
                workflow.workflow.describe()
            );
            if let Some(with) = &workflow.with {
                println!("{inner}with: {with}");
            }
        }
        StepKind::Decide(decide) => {
            let questions: Vec<String> = decide
                .questions
                .iter()
                .map(|(id, question_type)| format!("{id} ({})", question_type.name()))
                .collect();
            println!("{inner}decide: {}", questions.join(", "));
            if let Some(model) = &decide.model {
                println!("{inner}model: {model}");
            }
        }
        StepKind::Jq(filter) => println!("{inner}jq: {filter}"),
        StepKind::Ask(ask) => {
            println!("{inner}ask: {}", preview(&ask.prompt));
            if let Some(choices) = &ask.choices {
                println!("{inner}choices: {}", choices.join(", "));
            }
            match &ask.default {
                Some(default) => println!(
                    "{inner}default: {default}  (used when stdin is not an interactive terminal)"
                ),
                None => println!(
                    "{inner}default: none  (fails when stdin is not an interactive terminal)"
                ),
            }
        }
        StepKind::Write(write) => println!("{inner}write: {}", preview(&write.path)),
        StepKind::Group(steps) => print_steps(steps, ctx, &inner, known_input)?,
        StepKind::Switch(switch) => {
            for (index, case) in switch.cases.iter().enumerate() {
                println!("{inner}case {}: when {}", index + 1, case.when);
                print_steps(&case.steps, ctx, &body, None)?;
            }
            match &switch.else_steps {
                Some(else_steps) => {
                    println!("{inner}else:");
                    print_steps(else_steps, ctx, &body, None)?;
                }
                None => println!("{inner}else: (none; no matching case is a runtime error)"),
            }
        }
        StepKind::Parallel(parallel) => {
            for (name, steps) in &parallel.branches {
                println!("{inner}branch '{name}':");
                print_steps(steps, ctx, &body, None)?;
            }
        }
        StepKind::ForEach(for_each) => {
            println!("{inner}items: {}", for_each.items);
            if for_each.max_concurrency > 1 {
                println!("{inner}max_concurrency: {}", for_each.max_concurrency);
            }
            print_steps(&for_each.steps, ctx, &body, None)?;
        }
        StepKind::Loop(loop_step) => {
            println!(
                "{inner}{} {}, max_iterations: {}",
                loop_step.condition.keyword(),
                loop_step.condition.filter(),
                loop_step.max_iterations
            );
            print_steps(&loop_step.steps, ctx, &body, None)?;
        }
        StepKind::Stop => println!("{inner}-> ends the workflow with the current value"),
        StepKind::Break => println!("{inner}-> ends the innermost loop with the current value"),
    }

    if let Some(output) = &step.output {
        println!("{inner}output: {output}");
    }
    if let Some(retry) = effective_retry(step, ctx.scope) {
        println!(
            "{inner}retry: max_attempts={}, delay_seconds={}, backoff={}",
            retry.max_attempts, retry.delay_seconds, retry.backoff
        );
    }
    if let Some(seconds) = effective_timeout(step, ctx.scope) {
        println!("{inner}timeout: {seconds}s");
    }
    if let Some(on_error) = &step.on_error {
        println!("{inner}on_error:");
        print_steps(on_error, ctx, &body, None)?;
    }
    Ok(())
}

/// Resolves and prints an LLM step's model/base_url and capabilities via the
/// same resolution a real run uses, without sending a request.
fn print_model(
    llm: &LlmOverrides,
    agent_file: Option<&agent::AgentFile>,
    ctx: &DryRunContext,
    label: &str,
    inner: &str,
) -> Result<()> {
    let settings = resolve_llm_settings(llm, agent_file, ctx.scope, ctx.file_config, label)?;
    println!(
        "{inner}model: {} @ {}",
        settings.resolved_model.model_id, settings.base_url
    );
    for (name, values) in [
        ("mcp", &settings.mcp),
        ("skills", &settings.skills),
        ("subagents", &settings.subagents),
        ("tools", &settings.tools),
    ] {
        if !values.is_empty() {
            println!("{inner}{name}: {}", values.join(", "));
        }
    }
    Ok(())
}

fn render_preview(source: &str, input: Option<&Value>, ctx: &DryRunContext) -> String {
    match template::render_preview(source, input, &ctx.scope.inputs) {
        Ok(rendered) => rendered,
        Err(_) => format!("{source}  [unrendered: depends on a value not known before the run]"),
    }
}
