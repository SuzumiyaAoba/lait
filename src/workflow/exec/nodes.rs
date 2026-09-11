//! Per-node-type execution: one `execute_<kind>` function per
//! `workflow::NodeDefinition` variant, dispatched by [`execute`] — the
//! single entry point `exec.rs`'s `execute_step` calls before applying the
//! node's own `jq`/`write_file` settings (which stay in `exec.rs`, since
//! they apply uniformly across every variant rather than belonging to any
//! one of them). Mirrors `routers.rs`'s shape: one dispatcher, one private
//! function per variant, variant-specific helpers kept alongside their own
//! function rather than in `exec.rs`.

use std::{borrow::Cow, path::Path};

use anyhow::{Context, Result};

use crate::{
    engine::{AgentTurn, PromptTurn, call_agent},
    response, schema, template, workflow,
};

use super::{
    RunStepsFrame, StepContext, StepContextExt, StepsOutcome, announce_named_file,
    resolve_attachments, resolve_step_settings, run_steps, validate_execution_placement,
};

/// Runs a single node (agent call, prompt call, sub-workflow, command, or
/// `jq`/`write_file`-only data transform) and returns its output. `label` is
/// the calling `use:` site's label, used only for progress output/error
/// messages.
pub(super) async fn execute(
    node: &workflow::NodeDefinition,
    current_input: &str,
    context: StepContext<'_>,
) -> Result<String> {
    match node {
        workflow::NodeDefinition::Prompt(prompt_node) => {
            execute_prompt(node, prompt_node, current_input, &context).await
        }
        workflow::NodeDefinition::Agent(agent_node) => {
            execute_agent(node, agent_node, current_input, &context).await
        }
        workflow::NodeDefinition::Workflow(workflow_node) => {
            execute_workflow(workflow_node, current_input, &context).await
        }
        workflow::NodeDefinition::Command(command_node) => {
            execute_command(command_node, current_input, &context).await
        }
        workflow::NodeDefinition::Transform(_) => Ok(current_input.to_string()),
        workflow::NodeDefinition::Ask(ask_node) => {
            execute_ask(ask_node, current_input, &context).await
        }
    }
}

async fn execute_prompt(
    node: &workflow::NodeDefinition,
    prompt_node: &workflow::PromptNode,
    current_input: &str,
    context: &StepContext<'_>,
) -> Result<String> {
    let StepContext {
        scope,
        env,
        label,
        steps_outputs,
        ..
    } = *context;
    // Parsed once and shared with the `RenderScope` built below —
    // `input_schema` validation and prompt/system_prompt rendering
    // both need the same parsed value.
    let input = template::parse_input(current_input);
    if let Some(name_or_path) = &prompt_node.input_schema {
        let schema = schema::resolve_named_schema_value_cancellable(
            &scope.json_schemas,
            name_or_path,
            context.step_cancel.clone(),
        )
        .await
        .step(label)?;
        schema::validate_input_against_schema(&schema, &input).step(label)?;
    }

    let settings = resolve_step_settings(node, scope, &env.services.file_config, None, label)?
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
                        context.step_cancel.clone(),
                    )
                    .await
                }
                None => {
                    schema::load_json_schema_cancellable(
                        Path::new(name_or_path),
                        schema_name,
                        context.step_cancel.clone(),
                    )
                    .await
                }
            };
            Some(response_format.step(label)?)
        }
        None => None,
    };

    // Built once and shared by both renders below: handlebars'
    // `Context` owns a clone of `input`/`steps_outputs`/`env.vars`,
    // so rendering `prompt`/`system_prompt` through one scope
    // clones that data once instead of once per template.
    let render_scope = template::RenderScope::new(&input, steps_outputs, &env.vars);
    // A `system_prompt`-only node (no `prompt`) sends the current
    // input unchanged as the user message, the same way an `agent`
    // node's `current_input` passes straight through `call_agent`
    // without going through `template::render`.
    let prompt: Cow<'_, str> = match &prompt_node.prompt {
        Some(prompt_template) => Cow::Owned(render_scope.render(prompt_template).step(label)?),
        None => Cow::Borrowed(current_input),
    };
    let (prompt, image_urls) = resolve_attachments(
        prompt_node.files.as_deref(),
        prompt_node.images.as_deref(),
        &prompt,
        label,
        context.step_cancel.clone(),
    )
    .await?;
    let system_prompt = prompt_node
        .system_prompt
        .as_deref()
        .or(scope.defaults.system_prompt.as_deref())
        .map(|system_prompt_template| render_scope.render(system_prompt_template))
        .transpose()
        .step(label)?;

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
            context.step_cancel.clone(),
        )
        .await
        .step(label)?;

    response::render_response(&response, false, false).step(label)
}

async fn execute_agent(
    node: &workflow::NodeDefinition,
    agent_node: &workflow::AgentNode,
    current_input: &str,
    context: &StepContext<'_>,
) -> Result<String> {
    let StepContext {
        scope,
        env,
        label,
        steps_outputs,
        ..
    } = *context;
    // Loaded through the registry's path cache (not
    // `agent::load_agent` directly) so a `for_each`/`loop` body
    // re-running this node reuses the parsed file and its resolved
    // input schema instead of re-reading both from disk on every
    // iteration.
    let loaded = env
        .services
        .agent_registry
        .load_path_cancellable(&agent_node.agent, context.step_cancel.clone())
        .await
        .step(label)?;
    let agent_file = &loaded.file;

    let input = template::parse_input(current_input);
    loaded.validate_input(&input).step(label)?;

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
        context.step_cancel.clone(),
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
        context.step_cancel.clone(),
    )
    .await
    .step(label)
}

async fn execute_workflow(
    workflow_node: &workflow::WorkflowNode,
    current_input: &str,
    context: &StepContext<'_>,
) -> Result<String> {
    let StepContext {
        scope,
        env,
        placement,
        label,
        progress_prefix,
        ..
    } = *context;
    // Resolve cycles before opening a child file: a recursive FIFO
    // reference must fail rather than waiting for a second writer.
    let resolved_path = scope
        .resolve_nested_path(&workflow_node.workflow, label, context.step_cancel.clone())
        .await?;
    // Cached by path for the lifetime of the run — a `for_each`/
    // `loop` body re-running this node reuses the parsed file
    // instead of re-reading and re-parsing the same YAML on every
    // iteration (see `workflow::WorkflowRegistry`).
    let sub_wf = env
        .services
        .workflow_registry
        .load_path_cancellable(&resolved_path, context.step_cancel.clone())
        .await
        .step(label)?;
    validate_execution_placement(&sub_wf.steps, placement)
        .with_context(|| format!("step '{label}': workflow '{}'", resolved_path.display()))?;
    let sub_scope = scope.nested(resolved_path, &sub_wf);
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
            cancellation: context.step_cancel.clone(),
            placement,
        },
    )
    .await
    .step(label)?;
    Ok(result)
}

async fn execute_command(
    command_node: &workflow::CommandNode,
    current_input: &str,
    context: &StepContext<'_>,
) -> Result<String> {
    let StepContext {
        env,
        label,
        steps_outputs,
        ..
    } = *context;
    let input = template::parse_input(current_input);
    // One `RenderScope` for every argv element, instead of
    // rebuilding a handlebars `Context` (a clone of `input`/
    // `steps_outputs`/`env.vars`) per element.
    let render_scope = template::RenderScope::new(&input, steps_outputs, &env.vars);
    let rendered_argv: Vec<String> = command_node
        .command
        .iter()
        .map(|arg| render_scope.render(arg))
        .collect::<Result<_>>()
        .step(label)?;
    crate::process::run_command(&rendered_argv, current_input, context.step_cancel.clone())
        .await
        .step(label)
}

async fn execute_ask(
    ask_node: &workflow::AskNode,
    current_input: &str,
    context: &StepContext<'_>,
) -> Result<String> {
    let StepContext {
        env,
        label,
        steps_outputs,
        ..
    } = *context;
    let input = template::parse_input(current_input);
    let prompt =
        template::render(&ask_node.prompt, &input, steps_outputs, &env.vars).step(label)?;
    workflow::ask::run_ask(&prompt, ask_node, context.step_cancel.clone())
        .await
        .step(label)
}
