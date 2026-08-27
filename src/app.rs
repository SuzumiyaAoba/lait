use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestToolMessageArgs, ChatCompletionTools, FunctionCall, ResponseFormat,
};
use futures_util::{StreamExt, TryStreamExt};

use crate::{
    agent::{self, AgentFile},
    cli::{AgentAction, ChatArgs, Cli, Command, RunArgs},
    cli::{AgentRunArgs, ReasoningEffort},
    config::{self, ConfigFile, ModelMap},
    jq, llm, mcp, response, schema, template, workflow,
};

const DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";

/// The maximum number of tool-call round trips a single completion request
/// may take (see `RequestSettings::complete`) before lait gives up and
/// errors instead of looping forever on a model that keeps calling tools.
/// Overridable per CLI invocation/agent file/workflow node/`default:` via
/// `max_tool_rounds`.
const DEFAULT_MAX_TOOL_ROUNDS: usize = 8;

pub(crate) async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Run(run_args)) => run_workflow(run_args, cli.no_config).await,
        Some(Command::Agent(agent_command)) => match agent_command.action {
            AgentAction::Run(args) => run_agent(args, cli.no_config).await,
        },
        None => run_chat(cli.chat, cli.no_config).await,
    }
}

/// The reasoning-effort/temperature/top_p/max_tokens knobs a caller (CLI
/// invocation, agent file, or workflow step) may set for a single completion
/// request. Bundled into one struct (rather than four positional parameters)
/// because every layer of `resolve_request_settings`'s fallback chain treats
/// them identically: each field falls back independently to the next layer,
/// unlike e.g. `workflow::RetryDefinition`, which falls back as a whole unit.
#[derive(Debug, Default, Clone, Copy)]
struct SamplingOverrides {
    reasoning_effort: Option<ReasoningEffort>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<u32>,
}

/// The `mcp`/`max_tool_rounds` knobs a caller may set for a single completion
/// request, bundled the same way as `SamplingOverrides` and for the same
/// reason (keeps `resolve_request_settings`'s argument count down; each
/// field falls back independently to `file_config.default`, not as a whole
/// unit).
#[derive(Debug, Default, Clone)]
struct McpOverrides {
    mcp: Option<Vec<String>>,
    max_tool_rounds: Option<usize>,
}

/// The model/base-URL/API-key/sampling settings for a single completion
/// request, after resolving aliases and applying every fallback layer.
struct RequestSettings {
    base_url: String,
    api_key: String,
    resolved_model: config::ResolvedModel,
    sampling: SamplingOverrides,
    /// Names of `mcp_servers:` entries whose tools this request may call.
    /// Empty means "no tools" — `complete`'s fast path then behaves exactly
    /// like a single-shot request always has.
    mcp: Vec<String>,
    max_tool_rounds: usize,
}

impl RequestSettings {
    /// Sends a completion request built from these settings, driving a
    /// tool-call loop when `self.mcp` names at least one MCP server: each
    /// round sends the growing message history to the model, and if it comes
    /// back with `tool_calls`, `registry` executes them and their results are
    /// appended as `tool`-role messages before the next round. Ends either
    /// when a round produces no `tool_calls` (the model's final answer) or
    /// after `self.max_tool_rounds` rounds, whichever comes first.
    ///
    /// `response_format` is withheld from every round while tools are still
    /// in play and only attached to the final, tool-free round: many
    /// OpenAI-compatible servers, given a strict `json_schema` response
    /// format, force schema-conforming output and never emit `tool_calls` at
    /// all, which would silently stop tools from ever firing. See
    /// `docs/usage/ja/mcp.md`.
    async fn complete(
        &self,
        registry: &mcp::McpRegistry,
        system_prompt: Option<&str>,
        prompt: &str,
        response_format: Option<ResponseFormat>,
    ) -> Result<response::ChatCompletionResponse> {
        if self.mcp.is_empty() {
            return self
                .complete_once(system_prompt, prompt, response_format, &[])
                .await;
        }

        let tool_set = registry.tools(&self.mcp).await?;
        let mut messages = llm::initial_messages(system_prompt, prompt)?;

        let mut round = 0usize;
        loop {
            round += 1;
            if round > self.max_tool_rounds {
                bail!(
                    "tool loop exceeded max_tool_rounds ({}) without the model producing a final response",
                    self.max_tool_rounds
                );
            }

            let response = llm::complete(llm::CompletionRequest {
                base_url: &self.base_url,
                api_key: &self.api_key,
                model_id: &self.resolved_model.model_id,
                reasoning_effort: self.sampling.reasoning_effort,
                temperature: self.sampling.temperature,
                top_p: self.sampling.top_p,
                max_tokens: self.sampling.max_tokens,
                response_format: None,
                messages: messages.clone(),
                tools: &tool_set.tools,
            })
            .await?;

            let tool_calls = response::first_message(&response)
                .and_then(|message| message.tool_calls.as_ref())
                .filter(|tool_calls| !tool_calls.is_empty());

            let Some(tool_calls) = tool_calls else {
                if response_format.is_none() {
                    return Ok(response);
                }
                // The model stopped calling tools; re-issue the same history
                // once more with `response_format` attached, now that doing
                // so can no longer suppress a tool call.
                return llm::complete(llm::CompletionRequest {
                    base_url: &self.base_url,
                    api_key: &self.api_key,
                    model_id: &self.resolved_model.model_id,
                    reasoning_effort: self.sampling.reasoning_effort,
                    temperature: self.sampling.temperature,
                    top_p: self.sampling.top_p,
                    max_tokens: self.sampling.max_tokens,
                    response_format,
                    messages,
                    tools: &[],
                })
                .await;
            };

            let tool_call_entries: Vec<ChatCompletionMessageToolCalls> = tool_calls
                .iter()
                .map(|tool_call| {
                    ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                        id: tool_call.id.clone(),
                        function: FunctionCall {
                            name: tool_call.function.name.clone(),
                            arguments: tool_call.function.arguments.clone(),
                        },
                    })
                })
                .collect();
            let mut assistant_message = ChatCompletionRequestAssistantMessageArgs::default();
            assistant_message.tool_calls(tool_call_entries);
            if let Some(content) =
                response::first_message(&response).and_then(|message| message.content())
            {
                assistant_message.content(content);
            }
            messages.push(ChatCompletionRequestMessage::from(
                assistant_message.build()?,
            ));

            for tool_call in tool_calls {
                let result = registry
                    .call(
                        &tool_set,
                        &tool_call.function.name,
                        &tool_call.function.arguments,
                    )
                    .await?;
                let tool_message = ChatCompletionRequestToolMessageArgs::default()
                    .content(result)
                    .tool_call_id(tool_call.id.clone())
                    .build()?;
                messages.push(ChatCompletionRequestMessage::from(tool_message));
            }
        }
    }

    /// A single non-looping completion request: `system_prompt`/`prompt` as
    /// the whole history, `tools` sent as-is (usually empty).
    async fn complete_once(
        &self,
        system_prompt: Option<&str>,
        prompt: &str,
        response_format: Option<ResponseFormat>,
        tools: &[ChatCompletionTools],
    ) -> Result<response::ChatCompletionResponse> {
        llm::complete(llm::CompletionRequest {
            base_url: &self.base_url,
            api_key: &self.api_key,
            model_id: &self.resolved_model.model_id,
            reasoning_effort: self.sampling.reasoning_effort,
            temperature: self.sampling.temperature,
            top_p: self.sampling.top_p,
            max_tokens: self.sampling.max_tokens,
            response_format,
            messages: llm::initial_messages(system_prompt, prompt)?,
            tools,
        })
        .await
    }

    /// Like [`RequestSettings::complete`], but requests a streamed response.
    /// Rejects `self.mcp` being non-empty: a streamed `tool_calls` field
    /// arrives as index-keyed fragments that must be reassembled before they
    /// can be routed to an MCP server, which lait does not yet do (see
    /// `docs/usage/ja/mcp.md`).
    async fn complete_stream(
        &self,
        system_prompt: Option<&str>,
        prompt: &str,
        response_format: Option<ResponseFormat>,
    ) -> Result<llm::CompletionStream> {
        if !self.mcp.is_empty() {
            bail!(
                "'--stream'/streaming is not supported together with 'mcp:' yet; drop one of them"
            );
        }
        llm::complete_stream(llm::CompletionRequest {
            base_url: &self.base_url,
            api_key: &self.api_key,
            model_id: &self.resolved_model.model_id,
            reasoning_effort: self.sampling.reasoning_effort,
            temperature: self.sampling.temperature,
            top_p: self.sampling.top_p,
            max_tokens: self.sampling.max_tokens,
            response_format,
            messages: llm::initial_messages(system_prompt, prompt)?,
            tools: &[],
        })
        .await
    }
}

/// Consumes `stream`, writing each chunk's content delta to stdout as it
/// arrives (flushed immediately, since stdout is line-buffered and a delta
/// rarely ends in a newline). When `show_reasoning` is set, reasoning deltas
/// are written first, formatted like `response::format_response` formats a
/// complete response: a `Reasoning:` header before the first reasoning
/// delta, then a blank line before the first content delta. Reasoning deltas
/// are dropped when `show_reasoning` is unset, same as the non-streaming
/// path. Fails, like `response::response_content`, if the stream ends
/// without ever producing content.
async fn stream_to_stdout(mut stream: llm::CompletionStream, show_reasoning: bool) -> Result<()> {
    use std::io::Write;

    let mut stdout = std::io::stdout();
    let mut wrote_reasoning = false;
    let mut wrote_content = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let (content, reasoning) = response::stream_chunk_deltas(&chunk);
        if show_reasoning && let Some(reasoning) = reasoning {
            if !wrote_reasoning {
                writeln!(stdout, "Reasoning:")?;
                wrote_reasoning = true;
            }
            write!(stdout, "{reasoning}")?;
            stdout.flush()?;
        }
        if let Some(content) = content {
            if wrote_reasoning && !wrote_content {
                write!(stdout, "\n\n")?;
            }
            write!(stdout, "{content}")?;
            stdout.flush()?;
            wrote_content = true;
        }
    }

    if !wrote_content {
        bail!("API response contained no content in its first choice");
    }
    println!();
    Ok(())
}

/// Renders an agent's system prompt against `input`, calls the model with
/// `prompt` as the user message, and renders the response. Shared by
/// `run_agent` and `execute_step`'s agent branch.
async fn call_agent(
    agent_file: &AgentFile,
    settings: &RequestSettings,
    registry: &mcp::McpRegistry,
    input: &serde_json::Value,
    prompt: &str,
    steps_outputs: &workflow::StepOutputs,
) -> Result<String> {
    let system_prompt = template::render(&agent_file.system_prompt_template, input, steps_outputs)?;
    let response_format = agent_file
        .structured_output
        .then(|| {
            schema::build_response_format_from_entry(
                agent_file.output_schema.as_ref().expect(
                    "load_agent validates structured_output implies output_schema is present",
                ),
                agent_file.schema_name(),
            )
        })
        .transpose()?;

    let response = settings
        .complete(registry, Some(&system_prompt), prompt, response_format)
        .await?;
    response::render_response(&response, false, false)
}

/// Resolves the settings for one completion request. `model_name` and every
/// field of `overrides` must already reflect the caller's own precedence
/// chain (e.g. step > agent > workflow default); this only adds the two
/// layers every caller shares: the resolved model's own defaults, then
/// `lait.config.yml`'s `default:` block. `local_models` is the alias map to
/// check before falling back to `file_config`'s (a workflow's embedded
/// `models:`, or empty when there is none). `mcp_overrides` follows the same
/// two-layer fallback (caller's own value, then `file_config.default`) —
/// there is no per-model-alias `mcp:`, unlike `reasoning_effort`/
/// `temperature`, since an MCP server has no natural connection to a model
/// definition.
fn resolve_request_settings(
    model_name: String,
    overrides: SamplingOverrides,
    base_url_override: Option<String>,
    api_key_override: Option<String>,
    mcp_overrides: McpOverrides,
    local_models: &ModelMap,
    file_config: &ConfigFile,
) -> Result<RequestSettings> {
    let resolved_model = match config::resolve_model_alias(&model_name, local_models)? {
        Some(resolved) => resolved,
        None => config::resolve_model(model_name, file_config)?,
    };
    // `${VAR}` placeholders are only expanded in values sourced from
    // `lait.config.yml`/a workflow's `models:` (see
    // `config::expand_env_placeholders`), never in a `--base-url`/`--api-key`
    // CLI override, which the shell already expands on its own.
    let resolved_base_url = resolved_model
        .base_url
        .as_deref()
        .map(config::expand_env_placeholders)
        .transpose()?;
    let config_base_url = file_config
        .base_url
        .as_deref()
        .map(config::expand_env_placeholders)
        .transpose()?;
    let base_url = base_url_override
        .or(resolved_base_url)
        .or(config_base_url)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let base_url = base_url.trim_end_matches('/').to_owned();
    if base_url.is_empty() {
        return Err(anyhow!("base URL must not be empty"));
    }
    let resolved_api_key = resolved_model
        .api_key
        .as_deref()
        .map(config::expand_env_placeholders)
        .transpose()?;
    let config_api_key = file_config
        .api_key
        .as_deref()
        .map(config::expand_env_placeholders)
        .transpose()?;
    let api_key = api_key_override
        .or(resolved_api_key)
        .or(config_api_key)
        .unwrap_or_else(|| {
            // async-openai always builds an Authorization header from its config.
            // LM Studio ignores the value, so use a non-empty dummy key when no
            // key was supplied instead of making local requests fail on an empty
            // header value.
            "lm-studio".to_owned()
        });
    let sampling = SamplingOverrides {
        reasoning_effort: overrides
            .reasoning_effort
            .or(resolved_model.reasoning_effort)
            .or(file_config.default.reasoning_effort),
        temperature: overrides
            .temperature
            .or(resolved_model.temperature)
            .or(file_config.default.temperature),
        top_p: overrides
            .top_p
            .or(resolved_model.top_p)
            .or(file_config.default.top_p),
        max_tokens: overrides
            .max_tokens
            .or(resolved_model.max_tokens)
            .or(file_config.default.max_tokens),
    };
    // Catches an out-of-range value from any layer `workflow::validate`
    // cannot see on its own (a config file's `models:`/`default:`), on top of
    // whatever it already rejected at workflow parse time for values sourced
    // from the workflow file itself. Named by the resolved `model_id` (rather
    // than e.g. the alias) since that identifies the request uniformly
    // whether the value came from a `models:` entry, `default:`, or an
    // override — all of which have already been merged by this point.
    llm::validate_sampling_params(
        sampling.temperature,
        sampling.top_p,
        sampling.max_tokens,
        &format!("the request for model '{}'", resolved_model.model_id),
    )?;

    let mcp = mcp_overrides
        .mcp
        .or_else(|| file_config.default.mcp.clone())
        .unwrap_or_default();
    let max_tool_rounds = mcp_overrides
        .max_tool_rounds
        .or(file_config.default.max_tool_rounds)
        .unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);
    if max_tool_rounds == 0 {
        bail!(
            "the request for model '{}' has 'max_tool_rounds: 0'; it must be at least 1",
            resolved_model.model_id
        );
    }

    Ok(RequestSettings {
        base_url,
        api_key,
        resolved_model,
        sampling,
        mcp,
        max_tool_rounds,
    })
}

async fn run_chat(chat: ChatArgs, no_config: bool) -> Result<()> {
    let prompt = chat.prompt.clone().ok_or_else(|| {
        anyhow!("a PROMPT is required; provide one, or use `lait run <FILE> <PROMPT>`")
    })?;

    let file_config = config::load_config(no_config)?;
    let model_name = chat
        .model
        .clone()
        .or_else(|| file_config.default.model.clone())
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "model is required; provide --model, set LLM_MODEL, or specify default.model in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
    let settings = resolve_request_settings(
        model_name,
        SamplingOverrides {
            reasoning_effort: chat.reasoning_effort,
            temperature: chat.temperature,
            top_p: chat.top_p,
            max_tokens: chat.max_tokens,
        },
        chat.base_url.clone(),
        chat.api_key.clone(),
        McpOverrides {
            mcp: (!chat.mcp.is_empty()).then(|| chat.mcp.clone()),
            max_tool_rounds: None,
        },
        &ModelMap::default(),
        &file_config,
    )?;

    let response_format = chat
        .json_schema
        .as_deref()
        .map(|path| schema::load_json_schema(path, &chat.schema_name))
        .transpose()?;

    if chat.stream {
        let stream = settings
            .complete_stream(None, &prompt, response_format)
            .await?;
        return stream_to_stdout(stream, chat.show_reasoning).await;
    }

    let registry = mcp::McpRegistry::new(file_config.mcp_servers.clone());
    let response = settings
        .complete(&registry, None, &prompt, response_format)
        .await?;

    let output = response::render_response(&response, chat.json, chat.show_reasoning)?;
    println!("{output}");
    Ok(())
}

async fn run_agent(args: AgentRunArgs, no_config: bool) -> Result<()> {
    let agent_file = agent::load_agent(&args.file)?;
    let file_config = config::load_config(no_config)?;

    if let Some(name) = &agent_file.name {
        match &agent_file.description {
            Some(description) => eprintln!("==> {name}: {description}"),
            None => eprintln!("==> {name}"),
        }
    }

    let input = template::parse_input(&args.input);
    agent_file
        .validate_input(&input)
        .with_context(|| format!("agent '{}'", args.file.display()))?;

    let model_name = agent_file
        .model
        .clone()
        .or_else(|| file_config.default.model.clone())
        .ok_or_else(|| {
            anyhow!(
                "model is required; set it in the agent frontmatter or default.model in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
    let settings = resolve_request_settings(
        model_name,
        SamplingOverrides {
            reasoning_effort: agent_file.reasoning_effort,
            temperature: agent_file.temperature,
            top_p: agent_file.top_p,
            max_tokens: agent_file.max_tokens,
        },
        None,
        None,
        McpOverrides {
            mcp: agent_file.mcp.clone(),
            max_tool_rounds: agent_file.max_tool_rounds,
        },
        &ModelMap::default(),
        &file_config,
    )?;

    let registry = mcp::McpRegistry::new(file_config.mcp_servers.clone());
    let output = call_agent(
        &agent_file,
        &settings,
        &registry,
        &input,
        &args.input,
        &workflow::StepOutputs::new(),
    )
    .await
    .with_context(|| format!("agent '{}'", args.file.display()))?;
    println!("{output}");
    Ok(())
}

async fn run_workflow(run_args: RunArgs, no_config: bool) -> Result<()> {
    let mut wf = workflow::load_workflow(&run_args.file)?;
    let file_config = config::load_config(no_config)?;

    if let Some(name) = &wf.name {
        match &wf.description {
            Some(description) => eprintln!("==> {name}: {description}"),
            None => eprintln!("==> {name}"),
        }
    }

    let scope = WorkflowScope::top_level(&mut wf, &run_args.file)?;
    let registry = mcp::McpRegistry::new(file_config.mcp_servers.clone());
    let env = RunEnv {
        file_config: &file_config,
        registry: &registry,
    };
    let (current_input, _, _, _) = run_steps(
        &wf.steps,
        run_args.prompt,
        &scope,
        &env,
        0,
        "",
        workflow::StepOutputs::new(),
    )
    .await?;
    println!("{current_input}");
    Ok(())
}

/// The maximum `workflow:` nesting depth (a workflow step calling another
/// workflow file, whose own steps may call another, ...), rejected as a
/// runtime error rather than left to overflow the stack or hang.
const MAX_WORKFLOW_DEPTH: usize = 32;

/// The loaded config file and the MCP registry for the whole `lait run`
/// invocation — unlike `WorkflowScope`, neither changes at a `workflow:`
/// nesting boundary, so the same `&RunEnv` flows unchanged through every
/// `run_steps`/`execute_step_with_retry`/`execute_step` call. Bundled into
/// one struct (rather than two parameters) purely to keep those functions'
/// argument counts under clippy's `too_many_arguments` threshold.
struct RunEnv<'a> {
    file_config: &'a ConfigFile,
    registry: &'a mcp::McpRegistry,
}

/// The default model/reasoning-effort, model aliases, and JSON schema
/// definitions currently in effect, plus enough bookkeeping to run a nested
/// `workflow:` step safely. Every `resolve_step_settings`/`execute_step` call
/// reads through this instead of a `&workflow::WorkflowFile` directly, so a
/// `workflow:` step's sub-workflow can see its own `default:`/`models:`/
/// `json_schemas:` first, falling back to its caller's (`nested` builds
/// that merge). `active_paths` records every workflow file currently
/// executing (canonicalized), to reject a `workflow:` cycle and to cap
/// nesting depth at `MAX_WORKFLOW_DEPTH`.
struct WorkflowScope {
    default_model: Option<String>,
    default_reasoning_effort: Option<ReasoningEffort>,
    /// Fallback sampling `temperature`/`top_p`/`max_tokens`, merged across
    /// `workflow:` nesting the same way as `default_model`/
    /// `default_reasoning_effort` (each falls back independently, not as a
    /// whole unit like `default_retry`).
    default_temperature: Option<f64>,
    default_top_p: Option<f64>,
    default_max_tokens: Option<u32>,
    /// Fallback `retry`/`timeout` for a step that calls a model
    /// (`prompt`/`agent`) and doesn't set its own (see
    /// `execute_step_with_retry`). Merged across `workflow:` nesting the same
    /// way as `default_model`/`default_reasoning_effort`: a sub-workflow's own
    /// `default.retry`/`default.timeout` wins, falling back to its caller's
    /// when unset.
    default_retry: Option<workflow::RetryDefinition>,
    default_timeout: Option<u64>,
    /// Fallback `mcp`/`max_tool_rounds` for a node that doesn't set its own
    /// (see `resolve_step_settings`). Merged across `workflow:` nesting the
    /// same way as `default_model`/`default_reasoning_effort`: each falls
    /// back independently, like `temperature`, not as a whole unit like
    /// `default_retry`.
    default_mcp: Option<Vec<String>>,
    default_max_tool_rounds: Option<usize>,
    models: ModelMap,
    json_schemas: schema::JsonSchemaMap,
    /// This scope's own `nodes:` map, resolved by every `steps[].use` in this
    /// file. Unlike `models`/`json_schemas`, a `workflow:` node's sub-scope
    /// does *not* fall back to this scope's `nodes` for entries it lacks —
    /// each workflow file's `use:` sites only ever see that file's own
    /// `nodes:` (see `WorkflowScope::nested`).
    nodes: workflow::NodeMap,
    /// Directory relative paths in this scope's workflow file (currently
    /// only `node.workflow`) are resolved against.
    base_dir: PathBuf,
    active_paths: Vec<PathBuf>,
}

impl WorkflowScope {
    /// The scope for the workflow file passed on the command line. Takes
    /// `wf.nodes` by move (via `mem::take`) rather than cloning it: `wf`'s
    /// `nodes` map is never read again after this call, only `wf.steps`
    /// (see `run_workflow`).
    fn top_level(wf: &mut workflow::WorkflowFile, file_path: &Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(file_path).with_context(|| {
            format!(
                "failed to resolve workflow file path '{}'",
                file_path.display()
            )
        })?;
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            default_model: wf.default.model.clone(),
            default_reasoning_effort: wf.default.reasoning_effort,
            default_temperature: wf.default.temperature,
            default_top_p: wf.default.top_p,
            default_max_tokens: wf.default.max_tokens,
            default_retry: wf.default.retry.clone(),
            default_timeout: wf.default.timeout,
            default_mcp: wf.default.mcp.clone(),
            default_max_tool_rounds: wf.default.max_tool_rounds,
            models: wf.models.clone(),
            json_schemas: wf.json_schemas.clone(),
            nodes: std::mem::take(&mut wf.nodes),
            base_dir,
            active_paths: vec![canonical],
        })
    }

    /// The scope for a `workflow:` node's sub-workflow: resolves
    /// `relative_path` (as given in the node) against this scope's
    /// `base_dir`, merges `sub_wf`'s `default`/`models`/`json_schemas` over
    /// this scope's (the sub-workflow's own entries win; an entry it doesn't
    /// define falls back to this scope's), takes `sub_wf`'s `nodes:` outright
    /// by move (no fallback — see `WorkflowScope::nodes` — and `sub_wf.nodes`
    /// is never read again after this call, only `sub_wf.steps`), and
    /// extends the cycle/depth bookkeeping. Fails if `relative_path`
    /// resolves to a workflow file already executing (a cycle) or nesting
    /// has reached `MAX_WORKFLOW_DEPTH`.
    fn nested(
        &self,
        relative_path: &Path,
        sub_wf: &mut workflow::WorkflowFile,
        label: &str,
    ) -> Result<Self> {
        let resolved_path = self.base_dir.join(relative_path);
        let canonical = std::fs::canonicalize(&resolved_path).with_context(|| {
            format!(
                "step '{label}': failed to resolve workflow file path '{}'",
                resolved_path.display()
            )
        })?;
        if self.active_paths.contains(&canonical) {
            bail!(
                "step '{label}': 'workflow: {}' would create a cycle ('{}' is already running)",
                relative_path.display(),
                canonical.display()
            );
        }
        if self.active_paths.len() >= MAX_WORKFLOW_DEPTH {
            bail!(
                "step '{label}': 'workflow:' nesting exceeded the maximum depth of {MAX_WORKFLOW_DEPTH}"
            );
        }

        let mut models = sub_wf.models.clone();
        for (name, definitions) in &self.models {
            models
                .entry(name.clone())
                .or_insert_with(|| definitions.clone());
        }
        let mut json_schemas = sub_wf.json_schemas.clone();
        for (name, entry) in &self.json_schemas {
            json_schemas
                .entry(name.clone())
                .or_insert_with(|| entry.clone());
        }
        let mut active_paths = self.active_paths.clone();
        active_paths.push(canonical.clone());
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(Self {
            default_model: sub_wf
                .default
                .model
                .clone()
                .or_else(|| self.default_model.clone()),
            default_reasoning_effort: sub_wf
                .default
                .reasoning_effort
                .or(self.default_reasoning_effort),
            default_temperature: sub_wf.default.temperature.or(self.default_temperature),
            default_top_p: sub_wf.default.top_p.or(self.default_top_p),
            default_max_tokens: sub_wf.default.max_tokens.or(self.default_max_tokens),
            default_retry: sub_wf
                .default
                .retry
                .clone()
                .or_else(|| self.default_retry.clone()),
            default_timeout: sub_wf.default.timeout.or(self.default_timeout),
            default_mcp: sub_wf
                .default
                .mcp
                .clone()
                .or_else(|| self.default_mcp.clone()),
            default_max_tool_rounds: sub_wf
                .default
                .max_tool_rounds
                .or(self.default_max_tool_rounds),
            models,
            json_schemas,
            nodes: std::mem::take(&mut sub_wf.nodes),
            base_dir,
            active_paths,
        })
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
enum Flow {
    Continue,
    Break,
    Stop,
}

/// The final input, the running progress counter, the `Flow` signal the run
/// ended with, and the named step outputs recorded along the way, returned by
/// `run_steps`.
type StepsOutcome = Result<(String, usize, Flow, workflow::StepOutputs)>;

/// Runs a sequence of steps (the workflow's top-level `steps`, the nested
/// `steps` of a `switch` case/`else`, or a `parallel` branch), returning the
/// final input and the running progress counter so nested calls keep
/// numbering `[n]` labels continuously across the whole executed path
/// (skipped steps still consume a number). `progress_prefix` is prepended to
/// every progress line, so a `parallel` branch's interleaved output stays
/// attributable to its branch; it is threaded through unchanged by `switch`
/// (only one case ever runs, so its numbering stays continuous with the
/// parent) but reset to a fresh branch-local prefix and counter by
/// `parallel` (every branch runs concurrently, so a single shared counter
/// would not reflect real execution order). `steps_outputs` is threaded the
/// same way as `current_input`/`counter` for a `switch` case, `loop`
/// iteration, or `for_each` item (each sees every id recorded so far, and its
/// own recordings flow to whatever runs after it), but is only ever cloned
/// into a `parallel` branch, never merged back: concurrently running branches
/// recording into a shared namespace would race, and there is no well-defined
/// "the" value for an id set differently by two branches. Boxed because a
/// `switch`/`parallel` step recurses into this function from within an
/// `async` body, which Rust cannot size otherwise.
fn run_steps<'a>(
    steps: &'a [workflow::FlowStep],
    current_input: String,
    scope: &'a WorkflowScope,
    env: &'a RunEnv<'a>,
    start_counter: usize,
    progress_prefix: &'a str,
    steps_outputs: workflow::StepOutputs,
) -> Pin<Box<dyn Future<Output = StepsOutcome> + 'a>> {
    Box::pin(async move {
        let mut current_input = current_input;
        let mut counter = start_counter;
        let mut steps_outputs = steps_outputs;
        for step in steps {
            counter += 1;
            let label = step.label_or(counter);

            // `validate::validate_steps` guarantees at most one of
            // `switch`/`parallel`/`loop`/`for_each` is set, so `router()`
            // (which just checks them in a fixed order) can't silently
            // prefer one over another here. Matched exhaustively (no `_`
            // arm) so a new router kind fails to compile here until handled.
            match step.router() {
                Some(workflow::Router::Switch(switch)) => {
                    eprintln!("{progress_prefix}[{counter}] {label}");

                    let mut matched = None;
                    for (case_index, case) in switch.cases.iter().enumerate() {
                        if workflow::eval_when(&case.when, &current_input, &steps_outputs)
                            .with_context(|| format!("step '{label}'"))?
                        {
                            let case_label = case
                                .id
                                .clone()
                                .unwrap_or_else(|| format!("case-{}", case_index + 1));
                            eprintln!("{progress_prefix}    -> case '{case_label}' matched");
                            matched = Some(
                                run_steps(
                                    &case.steps,
                                    current_input.clone(),
                                    scope,
                                    env,
                                    counter,
                                    progress_prefix,
                                    steps_outputs.clone(),
                                )
                                .await?,
                            );
                            break;
                        }
                    }
                    let (result, new_counter, flow, new_steps_outputs) = match matched {
                        Some(result) => result,
                        None => match &switch.else_steps {
                            Some(else_steps) => {
                                eprintln!(
                                    "{progress_prefix}    -> no case matched, running 'else'"
                                );
                                run_steps(
                                    else_steps,
                                    current_input.clone(),
                                    scope,
                                    env,
                                    counter,
                                    progress_prefix,
                                    steps_outputs.clone(),
                                )
                                .await?
                            }
                            None => {
                                bail!(
                                    "step '{label}': no case matched and no 'else' branch is defined"
                                )
                            }
                        },
                    };
                    current_input = result;
                    counter = new_counter;
                    steps_outputs = new_steps_outputs;
                    record_step_output(&mut steps_outputs, step, &current_input);
                    if flow != Flow::Continue {
                        return Ok((current_input, counter, flow, steps_outputs));
                    }
                    continue;
                }

                Some(workflow::Router::Parallel(parallel)) => {
                    eprintln!("{progress_prefix}[{counter}] {label}");
                    eprintln!(
                        "{progress_prefix}    -> running {} branches concurrently",
                        parallel.branches.len()
                    );

                    let branch_labels: Vec<String> = parallel
                        .branches
                        .iter()
                        .enumerate()
                        .map(|(index, branch)| branch.label(index))
                        .collect();
                    let branch_prefixes: Vec<String> = branch_labels
                        .iter()
                        .map(|branch_label| format!("{progress_prefix}[{branch_label}] "))
                        .collect();
                    let branch_futures = parallel.branches.iter().zip(&branch_prefixes).map(
                        |(branch, branch_prefix)| {
                            run_steps(
                                &branch.steps,
                                current_input.clone(),
                                scope,
                                env,
                                0,
                                branch_prefix,
                                steps_outputs.clone(),
                            )
                        },
                    );
                    let branch_results = futures_util::future::try_join_all(branch_futures).await?;

                    // `validate_steps` rejects `stop`/`break` anywhere inside a
                    // `parallel` branch, so every branch always finishes with
                    // `Flow::Continue`; only its output is used here. Each branch
                    // got its own clone of `steps_outputs` (see this function's
                    // doc comment), so whatever it recorded stays branch-local.
                    let mut joined = serde_json::Map::new();
                    for (branch_label, (branch_output, _, _, _)) in
                        branch_labels.into_iter().zip(branch_results)
                    {
                        joined.insert(branch_label, template::parse_input(&branch_output));
                    }
                    let joined_json = serde_json::to_string(&serde_json::Value::Object(joined))
                        .context("failed to serialize joined 'parallel' branch outputs")?;

                    eprintln!("{progress_prefix}    -> branches joined");

                    current_input = match &parallel.join {
                        Some(filter) => jq::apply(filter, &joined_json, &steps_outputs)
                            .with_context(|| format!("step '{label}'"))?,
                        None => joined_json,
                    };
                    record_step_output(&mut steps_outputs, step, &current_input);
                    continue;
                }

                Some(workflow::Router::Loop(loop_def)) => {
                    eprintln!("{progress_prefix}[{counter}] {label}");
                    // Validated by `validate::validate_steps`: exactly one of
                    // `while`/`until` is set, and `max_iterations` is `Some(n)` with n >= 1.
                    let max_iterations = loop_def
                        .max_iterations
                        .expect("loop.max_iterations is required by validate_steps");

                    let mut iteration_input = current_input.clone();
                    // Threaded continuously across iterations (like `switch`, unlike
                    // `parallel`'s per-branch reset): the loop body genuinely runs
                    // sequentially, so a single growing counter reflects real execution
                    // order.
                    let mut loop_counter = counter;
                    if let Some(while_cond) = &loop_def.r#while {
                        let mut iterations_run = 0usize;
                        loop {
                            let should_continue =
                                workflow::eval_when(while_cond, &iteration_input, &steps_outputs)
                                    .with_context(|| format!("step '{label}'"))?;
                            if !should_continue {
                                break;
                            }
                            if iterations_run >= max_iterations {
                                bail!(
                                    "step '{label}': 'loop' reached max_iterations ({max_iterations}) without satisfying 'while'"
                                );
                            }
                            iterations_run += 1;
                            eprintln!(
                                "{progress_prefix}    -> iteration {iterations_run}/{max_iterations}"
                            );
                            let (result, new_counter, flow, new_steps_outputs) = run_steps(
                                &loop_def.steps,
                                iteration_input.clone(),
                                scope,
                                env,
                                loop_counter,
                                progress_prefix,
                                steps_outputs.clone(),
                            )
                            .await?;
                            iteration_input = result;
                            loop_counter = new_counter;
                            steps_outputs = new_steps_outputs;
                            match flow {
                                Flow::Continue => {}
                                Flow::Break => break,
                                Flow::Stop => {
                                    return Ok((
                                        iteration_input,
                                        loop_counter,
                                        Flow::Stop,
                                        steps_outputs,
                                    ));
                                }
                            }
                        }
                    } else {
                        let until_cond = loop_def.until.as_ref().expect(
                            "loop.until is required by validate_steps when 'while' is unset",
                        );
                        let mut iterations_run = 0usize;
                        let mut satisfied = false;
                        while iterations_run < max_iterations {
                            iterations_run += 1;
                            eprintln!(
                                "{progress_prefix}    -> iteration {iterations_run}/{max_iterations}"
                            );
                            let (result, new_counter, flow, new_steps_outputs) = run_steps(
                                &loop_def.steps,
                                iteration_input.clone(),
                                scope,
                                env,
                                loop_counter,
                                progress_prefix,
                                steps_outputs.clone(),
                            )
                            .await?;
                            iteration_input = result;
                            loop_counter = new_counter;
                            steps_outputs = new_steps_outputs;
                            if flow == Flow::Stop {
                                return Ok((
                                    iteration_input,
                                    loop_counter,
                                    Flow::Stop,
                                    steps_outputs,
                                ));
                            }
                            if flow == Flow::Break {
                                // An explicit `break: true` ends the loop like a
                                // satisfied `until`, not like exhausting
                                // `max_iterations`.
                                satisfied = true;
                                break;
                            }
                            satisfied =
                                workflow::eval_when(until_cond, &iteration_input, &steps_outputs)
                                    .with_context(|| format!("step '{label}'"))?;
                            if satisfied {
                                break;
                            }
                        }
                        if !satisfied {
                            bail!(
                                "step '{label}': 'loop' reached max_iterations ({max_iterations}) without satisfying 'until'"
                            );
                        }
                    }
                    current_input = iteration_input;
                    counter = loop_counter;
                    record_step_output(&mut steps_outputs, step, &current_input);
                    continue;
                }

                Some(workflow::Router::ForEach(for_each)) => {
                    eprintln!("{progress_prefix}[{counter}] {label}");
                    let items_json = jq::apply_one(&for_each.items, &current_input, &steps_outputs)
                        .with_context(|| format!("step '{label}'"))?;
                    let items_value: serde_json::Value = serde_json::from_str(&items_json)
                        .with_context(|| {
                            format!(
                                "step '{label}': failed to parse 'for_each.items' output as JSON"
                            )
                        })?;
                    let items = items_value.as_array().cloned().ok_or_else(|| {
                        anyhow!("step '{label}': 'for_each.items' must produce a JSON array")
                    })?;

                    let max_concurrency = for_each.max_concurrency.unwrap_or(1);
                    let results: Vec<serde_json::Value> = if max_concurrency <= 1 {
                        eprintln!(
                            "{progress_prefix}    -> iterating over {} item(s)",
                            items.len()
                        );
                        let mut results = Vec::with_capacity(items.len());
                        // Threaded continuously across items, like `loop` (see its
                        // comment above): a sequential `for_each` (the default)
                        // runs its body one item at a time, so a single growing
                        // counter matches real execution order.
                        let mut for_each_counter = counter;
                        let mut stop_result = None;
                        for (item_index, item) in items.iter().enumerate() {
                            eprintln!(
                                "{progress_prefix}    -> item {}/{}",
                                item_index + 1,
                                items.len()
                            );
                            // A string item is passed through raw (like `parallel`'s
                            // `current_input`, and the inverse of `template::parse_input`
                            // used below for results), not re-quoted as JSON, so
                            // `{{ input }}` sees the same unquoted text everywhere else
                            // in the pipeline does.
                            let item_input = match item {
                                serde_json::Value::String(text) => text.clone(),
                                other => serde_json::to_string(other)
                                    .context("failed to serialize a 'for_each' item")?,
                            };
                            let (result, new_counter, flow, new_steps_outputs) = run_steps(
                                &for_each.steps,
                                item_input,
                                scope,
                                env,
                                for_each_counter,
                                progress_prefix,
                                steps_outputs.clone(),
                            )
                            .await?;
                            for_each_counter = new_counter;
                            steps_outputs = new_steps_outputs;
                            if flow == Flow::Stop {
                                stop_result = Some(result);
                                break;
                            }
                            results.push(template::parse_input(&result));
                            if flow == Flow::Break {
                                break;
                            }
                        }
                        counter = for_each_counter;
                        if let Some(result) = stop_result {
                            return Ok((result, counter, Flow::Stop, steps_outputs));
                        }
                        results
                    } else {
                        eprintln!(
                            "{progress_prefix}    -> iterating over {} item(s), up to {max_concurrency} concurrently",
                            items.len()
                        );
                        let item_inputs: Vec<String> = items
                            .iter()
                            .map(|item| match item {
                                serde_json::Value::String(text) => Ok(text.clone()),
                                other => serde_json::to_string(other)
                                    .context("failed to serialize a 'for_each' item"),
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let item_prefixes: Vec<String> = (0..item_inputs.len())
                            .map(|index| format!("{progress_prefix}[item-{}] ", index + 1))
                            .collect();
                        let item_futures = item_inputs.into_iter().zip(&item_prefixes).map(
                            |(item_input, item_prefix)| {
                                run_steps(
                                    &for_each.steps,
                                    item_input,
                                    scope,
                                    env,
                                    0,
                                    item_prefix,
                                    steps_outputs.clone(),
                                )
                            },
                        );
                        // `validate_steps` rejects `stop`/`break` inside a
                        // `for_each` body whose `max_concurrency` is above 1, for
                        // the same reason as a `parallel` branch: concurrently
                        // running items can't share a single well-defined "break
                        // this loop"/"stop the workflow" target. Each item also
                        // got its own clone of `steps_outputs` (see this
                        // function's doc comment), so nothing it records leaks
                        // back here.
                        let item_results: Vec<(String, usize, Flow, workflow::StepOutputs)> =
                            futures_util::stream::iter(item_futures)
                                .buffered(max_concurrency)
                                .try_collect()
                                .await?;
                        item_results
                            .into_iter()
                            .map(|(output, _, _, _)| template::parse_input(&output))
                            .collect()
                    };

                    let results_json = serde_json::to_string(&serde_json::Value::Array(results))
                        .context("failed to serialize 'for_each' results")?;

                    current_input = match &for_each.join {
                        Some(filter) => jq::apply(filter, &results_json, &steps_outputs)
                            .with_context(|| format!("step '{label}'"))?,
                        None => results_json,
                    };
                    record_step_output(&mut steps_outputs, step, &current_input);
                    continue;
                }

                None => {}
            }

            if let Some(when) = &step.when {
                let truthy = workflow::eval_when(when, &current_input, &steps_outputs)
                    .with_context(|| format!("step '{label}'"))?;
                if !truthy {
                    eprintln!("{progress_prefix}[{counter}] {label} (skipped)");
                    continue;
                }
            }

            eprintln!("{progress_prefix}[{counter}] {label}");
            current_input = match &step.r#use {
                None => current_input,
                Some(node_id) => {
                    // `validate::validate_steps` guarantees every `use:` site
                    // resolves against `scope.nodes` before execution starts.
                    let node = scope
                        .nodes
                        .get(node_id)
                        .expect("validate_steps guarantees 'use' resolves in 'nodes'");
                    let attempt_result = execute_step_with_retry(
                        node,
                        &current_input,
                        scope,
                        env,
                        &label,
                        progress_prefix,
                        &steps_outputs,
                    )
                    .await;
                    match attempt_result {
                        Ok(output) => output,
                        Err(error) => match &step.on_error {
                            Some(on_error) => {
                                eprintln!(
                                    "{progress_prefix}    -> step failed, running 'on_error': {error}"
                                );
                                let error_input = serde_json::json!({
                                    "error": error.to_string(),
                                    "input": template::parse_input(&current_input),
                                });
                                let error_input_json = serde_json::to_string(&error_input)
                                    .context("failed to serialize 'on_error' input")?;
                                let (result, new_counter, flow, new_steps_outputs) = run_steps(
                                    &on_error.steps,
                                    error_input_json,
                                    scope,
                                    env,
                                    counter,
                                    progress_prefix,
                                    steps_outputs.clone(),
                                )
                                .await?;
                                counter = new_counter;
                                steps_outputs = new_steps_outputs;
                                if flow != Flow::Continue {
                                    return Ok((result, counter, flow, steps_outputs));
                                }
                                result
                            }
                            None => return Err(error),
                        },
                    }
                }
            };

            record_step_output(&mut steps_outputs, step, &current_input);

            if step.r#break == Some(true) {
                return Ok((current_input, counter, Flow::Break, steps_outputs));
            }
            if step.stop == Some(true) {
                return Ok((current_input, counter, Flow::Stop, steps_outputs));
            }
        }
        Ok((current_input, counter, Flow::Continue, steps_outputs))
    })
}

/// Runs `execute_step`, applying an effective timeout to each attempt and
/// retrying per an effective `retry` on failure (a timed-out attempt counts
/// as a failure). "Effective" means the node's own `retry`/`timeout` if set,
/// else `scope`'s `default_retry`/`default_timeout` (see
/// `WorkflowScope::default_retry`) — but only for a node that calls a model
/// (`prompt`/`agent`): a `jq`-only or `workflow:` node never falls back to
/// the workflow default (a `workflow:` node's own `retry`/`timeout` are
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
    scope: &WorkflowScope,
    env: &RunEnv<'_>,
    label: &str,
    progress_prefix: &str,
    steps_outputs: &workflow::StepOutputs,
) -> Result<String> {
    let calls_model = node.prompt.is_some() || node.agent.is_some();
    let effective_retry = node.retry.as_ref().or(calls_model
        .then_some(scope.default_retry.as_ref())
        .flatten());
    let effective_timeout = node
        .timeout
        .or(calls_model.then_some(scope.default_timeout).flatten());

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
    );

    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let outcome = match effective_timeout {
            Some(seconds) => {
                match tokio::time::timeout(
                    Duration::from_secs(seconds),
                    execute_step(
                        node,
                        current_input,
                        scope,
                        env,
                        label,
                        progress_prefix,
                        steps_outputs,
                    ),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!(
                        "step '{label}' timed out after {seconds}s (attempt {attempt}/{max_attempts})"
                    )),
                }
            }
            None => {
                execute_step(
                    node,
                    current_input,
                    scope,
                    env,
                    label,
                    progress_prefix,
                    steps_outputs,
                )
                .await
            }
        };

        match outcome {
            Ok(output) => return Ok(output),
            Err(error) if attempt < max_attempts => {
                eprintln!(
                    "{progress_prefix}    -> attempt {attempt}/{max_attempts} failed: {error}; retrying in {:.1}s",
                    delay.as_secs_f64()
                );
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                delay = Duration::from_secs_f64((delay.as_secs_f64() * backoff).max(0.0));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Resolves the model/reasoning-effort settings for a node's model call,
/// applying the node > agent file (when this node has one) > workflow
/// default precedence chain shared by `execute_step`'s `agent` and `prompt`
/// branches. `agent_file` is `Some` only for an `agent` node; besides adding
/// its fallback layer, its presence also selects which hint text a
/// missing-model error uses.
fn resolve_step_settings(
    node: &workflow::NodeDefinition,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    agent_file: Option<&AgentFile>,
    label: &str,
) -> Result<RequestSettings> {
    let model_name = node
        .model
        .clone()
        .or_else(|| agent_file.and_then(|agent_file| agent_file.model.clone()))
        .or_else(|| scope.default_model.clone())
        .ok_or_else(|| {
            anyhow!(
                "model is required for step '{label}'; set it on the node,{} the workflow's default.model, or in {}",
                if agent_file.is_some() { " its agent file," } else { "" },
                config::CONFIG_FILE_NAME
            )
        })?;
    let overrides = SamplingOverrides {
        reasoning_effort: node
            .reasoning_effort
            .or(agent_file.and_then(|agent_file| agent_file.reasoning_effort))
            .or(scope.default_reasoning_effort),
        temperature: node
            .temperature
            .or(agent_file.and_then(|agent_file| agent_file.temperature))
            .or(scope.default_temperature),
        top_p: node
            .top_p
            .or(agent_file.and_then(|agent_file| agent_file.top_p))
            .or(scope.default_top_p),
        max_tokens: node
            .max_tokens
            .or(agent_file.and_then(|agent_file| agent_file.max_tokens))
            .or(scope.default_max_tokens),
    };
    let mcp = node
        .mcp
        .clone()
        .or_else(|| agent_file.and_then(|agent_file| agent_file.mcp.clone()))
        .or_else(|| scope.default_mcp.clone());
    let max_tool_rounds = node
        .max_tool_rounds
        .or_else(|| agent_file.and_then(|agent_file| agent_file.max_tool_rounds))
        .or(scope.default_max_tool_rounds);
    resolve_request_settings(
        model_name,
        overrides,
        None,
        None,
        McpOverrides {
            mcp,
            max_tool_rounds,
        },
        &scope.models,
        file_config,
    )
    .with_context(|| format!("step '{label}'"))
}

/// Runs a single node (agent call, prompt call, or `jq`-only data transform)
/// and returns its output, with `jq` applied afterward if set. `label` is the
/// calling `use:` site's label, used only for progress output/error messages.
async fn execute_step(
    node: &workflow::NodeDefinition,
    current_input: &str,
    scope: &WorkflowScope,
    env: &RunEnv<'_>,
    label: &str,
    progress_prefix: &str,
    steps_outputs: &workflow::StepOutputs,
) -> Result<String> {
    if let Some(name_or_path) = &node.input_schema {
        let schema = schema::resolve_named_schema_value(&scope.json_schemas, name_or_path)
            .with_context(|| format!("step '{label}'"))?;
        let input = template::parse_input(current_input);
        schema::validate_input_against_schema(&schema, &input)
            .with_context(|| format!("step '{label}'"))?;
    }

    let mut step_output = if let Some(agent_path) = &node.agent {
        let agent_file =
            agent::load_agent(agent_path).with_context(|| format!("step '{label}'"))?;

        let input = template::parse_input(current_input);
        agent_file
            .validate_input(&input)
            .with_context(|| format!("step '{label}'"))?;

        let settings =
            resolve_step_settings(node, scope, env.file_config, Some(&agent_file), label)?;

        call_agent(
            &agent_file,
            &settings,
            env.registry,
            &input,
            current_input,
            steps_outputs,
        )
        .await
        .with_context(|| format!("step '{label}'"))?
    } else if let Some(prompt_template) = &node.prompt {
        let settings = resolve_step_settings(node, scope, env.file_config, None, label)?;

        let response_format = node
            .output_schema
            .as_deref()
            .map(|name_or_path| {
                let schema_name = node.schema_name.as_deref().unwrap_or("structured_output");
                match scope.json_schemas.get(name_or_path) {
                    Some(entry) => schema::build_response_format_from_entry(entry, schema_name),
                    None => {
                        schema::load_json_schema(std::path::Path::new(name_or_path), schema_name)
                    }
                }
            })
            .transpose()
            .with_context(|| format!("step '{label}'"))?;

        let input = template::parse_input(current_input);
        let prompt = template::render(prompt_template, &input, steps_outputs)
            .with_context(|| format!("step '{label}'"))?;

        let response = settings
            .complete(env.registry, None, &prompt, response_format)
            .await
            .with_context(|| format!("step '{label}'"))?;

        response::render_response(&response, false, false)
            .with_context(|| format!("step '{label}'"))?
    } else if let Some(sub_workflow_path) = &node.workflow {
        let resolved_path = scope.base_dir.join(sub_workflow_path);
        let mut sub_wf =
            workflow::load_workflow(&resolved_path).with_context(|| format!("step '{label}'"))?;
        let sub_scope = scope.nested(sub_workflow_path, &mut sub_wf, label)?;
        if let Some(name) = &sub_wf.name {
            match &sub_wf.description {
                Some(description) => eprintln!("{progress_prefix}    -> {name}: {description}"),
                None => eprintln!("{progress_prefix}    -> {name}"),
            }
        }
        // Isolated like an `agent:` call, not threaded like a `switch` case:
        // the sub-workflow is a separate file with its own step ids, so it
        // starts with an empty `steps_outputs` and its Flow (whether it
        // ended via `stop`/`break` internally or just ran out of steps) is
        // this step's own concern, not the caller's — only its final output
        // crosses back.
        let sub_progress_prefix = format!("{progress_prefix}    ");
        let (result, ..) = run_steps(
            &sub_wf.steps,
            current_input.to_string(),
            &sub_scope,
            env,
            0,
            &sub_progress_prefix,
            workflow::StepOutputs::new(),
        )
        .await
        .with_context(|| format!("step '{label}'"))?;
        result
    } else {
        current_input.to_string()
    };

    if let Some(filter) = &node.jq {
        step_output = jq::apply(filter, &step_output, steps_outputs)
            .with_context(|| format!("step '{label}'"))?;
    }

    if let Some(path) = &node.write_file {
        std::fs::write(path, &step_output).with_context(|| {
            format!(
                "step '{label}': failed to write output to '{}'",
                path.display()
            )
        })?;
    }

    Ok(step_output)
}
