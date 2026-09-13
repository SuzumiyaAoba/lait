//! Agent-file/subagent dispatch: rendering an agent's own template/schema/
//! completion pipeline (`call_agent`) and running one as a `subagents:` tool
//! call mid-completion (`call_subagent_tool`, mutually recursive with
//! `RequestSettings::complete` through `call_agent` — see its doc comment).
//! Shared by `app::run_agent`, a workflow node's `agent:` action
//! (`workflow::exec::nodes::execute_agent`), and `engine::tool_loop`'s
//! subagent-tool branch.

use std::{future::Future, path::PathBuf, pin::Pin};

use anyhow::{Context, Result, anyhow, bail};

use crate::{agent::AgentFile, nesting, response, schema, template, workflow};

use super::{PromptTurn, RequestSettings, RunContext, agent_file_settings};

/// The per-call inputs to `call_agent` beyond settings/env: the JSON-parsed
/// input (for rendering the agent's system prompt template), the raw text
/// sent as the user message, and any `--image`-style attachments for it.
/// Bundled, like `PromptTurn` above, to keep `call_agent`'s argument count
/// under clippy's `too_many_arguments` threshold.
pub(crate) struct AgentTurn<'a> {
    pub(crate) input: &'a serde_json::Value,
    pub(crate) prompt: &'a str,
    pub(crate) image_urls: &'a [String],
}

impl<'a> AgentTurn<'a> {
    /// A turn with no image attachments — every caller but `execute_step`'s
    /// agent branch, which has a node's own `images:` to resolve.
    pub(crate) fn simple(input: &'a serde_json::Value, prompt: &'a str) -> Self {
        Self {
            input,
            prompt,
            image_urls: &[],
        }
    }
}

/// Renders an agent's system prompt against `turn.input`, calls the model
/// with `turn.prompt` as the user message, and renders the response. Shared
/// by `run_agent`, `execute_step`'s agent branch, and `call_subagent_tool`.
/// `active_agent_paths` is threaded straight through to `settings.complete`
/// — see its doc comment; every caller but `call_subagent_tool` passes `&[]`.
pub(crate) async fn call_agent(
    agent_file: &AgentFile,
    settings: &RequestSettings,
    env: &RunContext,
    turn: AgentTurn<'_>,
    steps_outputs: &workflow::StepOutputs,
    active_agent_paths: &[PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let system_prompt = template::render(
        &agent_file.system_prompt_template,
        turn.input,
        steps_outputs,
        &env.vars,
    )?;
    let response_format = if agent_file.structured_output {
        Some(
            schema::build_response_format_from_entry_cancellable(
                agent_file.output_schema.as_ref().expect(
                    "load_agent validates structured_output implies output_schema is present",
                ),
                agent_file.schema_name(),
                cancellation.clone(),
            )
            .await?,
        )
    } else {
        None
    };

    let response = settings
        .complete(
            env,
            active_agent_paths,
            PromptTurn {
                system_prompt: Some(&system_prompt),
                history: &[],
                prompt: turn.prompt,
                image_urls: turn.image_urls,
            },
            response_format,
            cancellation,
        )
        .await?;
    response::render_plain(&response)
}

/// The maximum recursive subagent-calling depth (a subagent whose own
/// `subagents:` names another, whose own names another, ...), rejected as a
/// runtime error the same way `MAX_WORKFLOW_DEPTH` rejects excessive
/// `workflow:` nesting.
const MAX_SUBAGENT_DEPTH: usize = 16;

/// Converts a JSON value into raw prompt/tool-input text: a `Value::String`
/// passes through unquoted (so `{{ input }}` sees the same plain text
/// everywhere else in the pipeline does), any other value (object, array,
/// number, ...) is serialized to compact JSON text. `context` names the
/// caller's own site for the serialization-failure error. Shared by
/// `run_steps`' `for_each` branch (both the sequential and concurrent
/// per-item conversion) and `subagent_tool_input` below — the same "is this
/// already plain text, or does it need to become a JSON text blob" question
/// both ask.
pub(crate) fn value_to_input_text(
    value: &serde_json::Value,
    context: &'static str,
) -> Result<String> {
    match value {
        serde_json::Value::String(text) => Ok(text.clone()),
        other => serde_json::to_string(other).context(context),
    }
}

/// Unwraps a subagent tool call's raw JSON `arguments` into the `(input,
/// prompt)` pair `call_agent` needs — the parsed JSON value for `{{
/// input.field }}` template access, and the raw text sent as the user-role
/// message. Mirrors `subagent::AgentRegistry::tools`' two parameter shapes:
/// when `file` declares an `input_schema`, the whole `arguments` object *is*
/// the subagent's input (its own schema already shaped `parameters`, so
/// there's nothing to unwrap) and `prompt` is its canonical JSON text;
/// otherwise `arguments` is the generic `{ "input": ... }` wrapper, and
/// `input`/`prompt` are read out of its `input` field via
/// `value_to_input_text`.
fn subagent_tool_input(
    file: &AgentFile,
    arguments_json: &str,
) -> Result<(serde_json::Value, String)> {
    let arguments: serde_json::Value = if arguments_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments_json)
            .context("failed to parse subagent tool call arguments as JSON")?
    };

    if file.input_schema.is_some() {
        let prompt = value_to_input_text(
            &arguments,
            "failed to serialize subagent tool call arguments",
        )?;
        return Ok((arguments, prompt));
    }

    // Moved out (not cloned) when `arguments` is an object, since `arguments`
    // itself is never used again after this.
    let input_value = match arguments {
        serde_json::Value::Object(mut map) => map.remove("input"),
        _ => None,
    }
    .ok_or_else(|| anyhow!("subagent tool call is missing the required 'input' field"))?;
    let prompt = value_to_input_text(
        &input_value,
        "failed to serialize subagent tool call 'input'",
    )?;
    Ok((input_value, prompt))
}

/// Runs subagent `name` (resolved via `env.services.agent_registry`, an `agents:`
/// entry) against one tool call's raw JSON `arguments`, recursively driving
/// its own completion (and, if it declares `subagents:`/`mcp:` of its own,
/// its own tool loop) to completion, and returns its rendered response text —
/// the shape a `tool`-role message needs. `active_paths` is every subagent
/// file already executing on this call stack (canonicalized); calling a
/// subagent already on it (a cycle) or beyond `MAX_SUBAGENT_DEPTH` is
/// rejected the same way `WorkflowScope`/`check_workflow_nesting` reject
/// excessive `workflow:` nesting. Boxed because this is mutually recursive
/// with `RequestSettings::complete` through `call_agent`, which Rust's
/// `async fn` cannot size otherwise.
pub(crate) fn call_subagent_tool<'a>(
    name: &'a str,
    arguments_json: &'a str,
    env: &'a RunContext,
    active_paths: &'a [PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        // `Copy` (it only captures `name: &str`), so it can back every
        // `with_context` call below without re-typing the same `format!`.
        let context = || format!("subagent '{name}'");

        let loaded = env
            .services
            .agent_registry
            .load_cancellable(name, cancellation.clone())
            .await?;

        if let Err(error) =
            nesting::check_nesting_depth(active_paths, &loaded.canonical_path, MAX_SUBAGENT_DEPTH)
        {
            match error {
                nesting::NestingDepthError::Cycle => bail!(
                    "calling subagent '{name}' would create a cycle ('{}' is already running)",
                    loaded.canonical_path.display()
                ),
                nesting::NestingDepthError::TooDeep => bail!(
                    "calling subagent '{name}' exceeded the maximum subagent nesting depth of \
                     {MAX_SUBAGENT_DEPTH}"
                ),
            }
        }

        let (input, prompt) =
            subagent_tool_input(&loaded.file, arguments_json).with_context(context)?;
        loaded.validate_input(&input).with_context(context)?;

        let settings = agent_file_settings(&loaded.file, &env.services.file_config, Some(name))
            .with_context(context)?
            .with_usage_label(format!("subagent '{name}'"));

        let mut next_active_paths = active_paths.to_vec();
        next_active_paths.push(loaded.canonical_path.clone());

        call_agent(
            &loaded.file,
            &settings,
            env,
            AgentTurn::simple(&input, &prompt),
            &workflow::StepOutputs::new(),
            &next_active_paths,
            cancellation,
        )
        .await
        .with_context(context)
    })
}
