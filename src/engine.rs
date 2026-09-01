//! The LLM request/response layer: resolving a completion request's
//! settings, sending it (including the MCP/subagent tool-call loop), and
//! calling an agent file's own template/schema/completion pipeline. Shared by
//! every caller that ultimately talks to a model — chat, `lait prompt`,
//! `lait agent run`, and a workflow node's `agent`/`prompt` action.

use std::{
    borrow::Cow,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat,
};
use futures_util::StreamExt;

use crate::{
    agent::AgentFile,
    cli::ReasoningEffort,
    config::{self, ConfigFile, ModelMap},
    llm, mcp, nesting, response, schema, skill, subagent, template, usage, workflow,
};

/// The maximum number of tool-call round trips a single completion request
/// may take (see `RequestSettings::complete`) before lait gives up and
/// errors instead of looping forever on a model that keeps calling tools.
/// Overridable per CLI invocation/agent file/workflow node/`default:` via
/// `max_tool_rounds`.
const DEFAULT_MAX_TOOL_ROUNDS: usize = 8;

/// The loaded config file, the MCP registry, the skill cache, the subagent
/// registry, and the run's top-level cancellation source for the whole
/// `lait`/`lait agent run`/`lait run` invocation — unlike `WorkflowScope`,
/// none of these change at a `workflow:` nesting boundary, so the same
/// `&AppContext` flows unchanged through every
/// `run_steps`/`execute_step_with_retry`/`execute_step` call (and, for
/// `call_agent`/`RequestSettings::complete`, through a subagent call's own
/// recursive completion too — see `call_subagent_tool`). Bundled into one
/// struct (rather than five parameters) purely to keep those functions'
/// argument counts under clippy's `too_many_arguments` threshold. Owns
/// everything (an `Arc<ConfigFile>`, not a borrow) rather than borrowing
/// `file_config` for a lifetime, so an `Arc<AppContext>` clone can move into
/// a `tokio::spawn`ed task for `parallel`/concurrent `for_each` (see
/// `workflow::exec::run_steps`) without that task's future needing to
/// outlive a borrow.
pub(crate) struct AppContext {
    pub(crate) file_config: Arc<ConfigFile>,
    pub(crate) registry: mcp::McpRegistry,
    pub(crate) skill_cache: skill::SkillCache,
    pub(crate) agent_registry: subagent::AgentRegistry,
    /// Every completion request's server-reported token usage, recorded by
    /// `RequestSettings::complete` and summarized when `--show-usage` asks
    /// for it.
    pub(crate) usage: usage::UsageTally,
    /// This invocation's own cancellation source, if any — the value every
    /// top-level `run_steps`/`complete` call seeds its own cancellation
    /// chain from (a node's own `timeout`/nested `workflow:` call then
    /// derives further child tokens off of that seed, see
    /// `execute_step_with_retry`). Currently always `None`: no caller wires
    /// up a real source (e.g. Ctrl-C) yet, but giving it one field here
    /// means a future one only has to change `new`'s caller, not every
    /// `run_steps`/`complete` call site.
    pub(crate) cancel: Option<tokio_util::sync::CancellationToken>,
}

impl AppContext {
    /// Builds the registries/cache over `file_config`'s named entries. Cheap:
    /// each registry gets its own `Arc` clone of just the map it needs (MCP
    /// connections, skill files, and agent files are all loaded lazily on
    /// first use, so cloning the (typically small) name/path maps up front
    /// costs far less than any of that).
    pub(crate) fn new(file_config: Arc<ConfigFile>) -> Self {
        Self {
            registry: mcp::McpRegistry::new(Arc::new(file_config.mcp_servers.clone())),
            skill_cache: skill::SkillCache::new(Arc::new(file_config.skills.clone())),
            agent_registry: subagent::AgentRegistry::new(Arc::new(file_config.agents.clone())),
            file_config,
            usage: usage::UsageTally::default(),
            cancel: None,
        }
    }

    /// Drives `fut` to completion, then unconditionally shuts down the MCP
    /// registry before handing back `fut`'s result — on success or failure
    /// alike, so callers don't have to re-derive that ordering themselves.
    /// Every top-level `lait`/`lait agent run`/`lait run` invocation must
    /// call this once, at the end, instead of awaiting its work directly.
    pub(crate) async fn finish<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        let result = fut.await;
        self.registry.shutdown().await;
        result
    }
}

/// The reasoning-effort/temperature/top_p/max_tokens knobs a caller (CLI
/// invocation, agent file, or workflow step) may set for a single completion
/// request. Bundled into one struct (rather than four positional parameters)
/// because every layer of `resolve_request_settings`'s fallback chain treats
/// them identically: each field falls back independently to the next layer,
/// unlike e.g. `workflow::RetryDefinition`, which falls back as a whole unit.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SamplingOverrides {
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
}

impl SamplingOverrides {
    /// Folds `layers` field by field in priority order (the first layer with
    /// a field set wins, independently per field) — the shared
    /// implementation behind every caller's own precedence chain:
    /// `resolve_chat_settings`'s single `SharedChatArgs` layer,
    /// `resolve_step_settings`'s node > agent file > workflow default, and
    /// `agent_file_settings`'s single frontmatter layer. Does not include the
    /// model-alias/`file_config.default` tail every caller shares —
    /// `resolve_request_settings` adds those two layers after this.
    pub(crate) fn fold(layers: &[Self]) -> Self {
        Self {
            reasoning_effort: layers.iter().find_map(|layer| layer.reasoning_effort),
            temperature: layers.iter().find_map(|layer| layer.temperature),
            top_p: layers.iter().find_map(|layer| layer.top_p),
            max_tokens: layers.iter().find_map(|layer| layer.max_tokens),
        }
    }
}

/// The `mcp`/`max_tool_rounds`/`skills`/`subagents` knobs a caller may set
/// for a single completion request, bundled the same way as
/// `SamplingOverrides` and for the same reason (keeps
/// `resolve_request_settings`'s argument count down; each field falls back
/// independently to `file_config.default`, not as a whole unit).
#[derive(Debug, Default, Clone)]
pub(crate) struct CapabilityOverrides {
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Option<Vec<String>>,
    pub(crate) subagents: Option<Vec<String>>,
}

impl CapabilityOverrides {
    /// Folds `layers` field by field in priority order — see
    /// `SamplingOverrides::fold`, which this mirrors.
    pub(crate) fn fold(layers: &[Self]) -> Self {
        Self {
            mcp: layers.iter().find_map(|layer| layer.mcp.clone()),
            max_tool_rounds: layers.iter().find_map(|layer| layer.max_tool_rounds),
            skills: layers.iter().find_map(|layer| layer.skills.clone()),
            subagents: layers.iter().find_map(|layer| layer.subagents.clone()),
        }
    }
}

/// The new-turn inputs shared by `RequestSettings::complete`/
/// `complete_stream`: the system prompt, any prior turns from a resumed
/// `--session` (empty for every caller but chat), the new user-role prompt
/// text, and any `--image` attachments for it (empty for every caller but
/// chat). Bundled into one struct, like `SamplingOverrides`/
/// `CapabilityOverrides` above, to keep `complete`'s argument count under
/// clippy's `too_many_arguments` threshold.
pub(crate) struct PromptTurn<'a> {
    pub(crate) system_prompt: Option<&'a str>,
    pub(crate) history: &'a [ChatCompletionRequestMessage],
    pub(crate) prompt: &'a str,
    pub(crate) image_urls: &'a [String],
}

impl<'a> PromptTurn<'a> {
    /// A turn with no prior history and no image attachments — every caller
    /// but chat's own (`run_chat`/`repl::run_turn`, which have a real
    /// `--session`/`--image` history to carry).
    pub(crate) fn simple(system_prompt: Option<&'a str>, prompt: &'a str) -> Self {
        Self {
            system_prompt,
            history: &[],
            prompt,
            image_urls: &[],
        }
    }
}

/// The model/base-URL/API-key/sampling settings for a single completion
/// request, after resolving aliases and applying every fallback layer.
pub(crate) struct RequestSettings {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) resolved_model: config::ResolvedModel,
    pub(crate) sampling: SamplingOverrides,
    /// Names of `mcp_servers:` entries whose tools this request may call.
    /// Empty means "no tools" — `complete`'s fast path then behaves exactly
    /// like a single-shot request always has.
    pub(crate) mcp: Vec<String>,
    pub(crate) max_tool_rounds: usize,
    /// Names of `skills:` entries whose content is appended to this
    /// request's system prompt (see `with_skills`). Empty means no skill
    /// content is appended.
    pub(crate) skills: Vec<String>,
    /// Names of `agents:` entries made available as callable subagent tools
    /// during this request's tool loop. Empty means "no subagent tools" —
    /// combined with `mcp` the same way in `complete`'s tool loop (empty
    /// tool sources for both keeps `complete`'s fast, tool-free path).
    pub(crate) subagents: Vec<String>,
    /// Names these settings' requests in `env.usage`'s `--show-usage`
    /// summary (a step label, an agent name, `"chat"`); every round of a
    /// tool loop records under the same label. Set via `with_usage_label`
    /// right after resolving, where the caller still knows what it is
    /// resolving for.
    pub(crate) usage_label: String,
}

/// Combines a caller's own system prompt (an agent's rendered template, or
/// `None` for a plain `prompt:`/chat call) with a request's rendered skill
/// content, if any (see `skill::SkillCache::render`). The skill content is
/// appended after `base`, under a `---` delimiter, so the caller's own
/// instructions lead and a skill's own Markdown heading structure stays
/// visually distinct from them. Returns `base` unchanged (borrowed, no copy)
/// when there's no skill content to append — the common case, since most
/// requests don't set `skills:`.
fn with_skills<'a>(base: Option<&'a str>, skills_text: Option<&str>) -> Option<Cow<'a, str>> {
    match (base, skills_text) {
        (None, None) => None,
        (Some(base), None) => Some(Cow::Borrowed(base)),
        (None, Some(skills_text)) => Some(Cow::Owned(skills_text.to_owned())),
        (Some(base), Some(skills_text)) => {
            Some(Cow::Owned(format!("{base}\n\n---\n\n{skills_text}")))
        }
    }
}

impl RequestSettings {
    /// Sets `usage_label` — see that field's doc comment.
    pub(crate) fn with_usage_label(mut self, label: impl Into<String>) -> Self {
        self.usage_label = label.into();
        self
    }

    /// Builds an `llm::CompletionRequest` from these settings plus the
    /// per-call `response_format`/`messages`/`tools`. The `base_url`/
    /// `api_key`/`model_id`/sampling fields are the same for every request
    /// `self` ever builds, so both `complete`'s tool loop and
    /// `complete_stream` go through here instead of repeating that field
    /// list at each call site.
    fn request<'a>(
        &'a self,
        response_format: Option<ResponseFormat>,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: &'a [ChatCompletionTools],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> llm::CompletionRequest<'a> {
        llm::CompletionRequest {
            base_url: &self.base_url,
            api_key: &self.api_key,
            model_id: &self.resolved_model.model_id,
            reasoning_effort: self.sampling.reasoning_effort,
            temperature: self.sampling.temperature,
            top_p: self.sampling.top_p,
            max_tokens: self.sampling.max_tokens,
            response_format,
            messages,
            tools,
            stream_include_usage: false,
            cancellation,
        }
    }

    /// Sends a completion request built from these settings, driving a
    /// tool-call loop when `self.mcp`/`self.subagents` names at least one MCP
    /// server or subagent: each round sends the growing message history to
    /// the model, and if it comes back with `tool_calls`, `env.registry`
    /// (for an MCP tool) or `call_subagent_tool` (for a subagent tool)
    /// executes them and their results are appended as `tool`-role messages
    /// before the next round. Ends either when a round produces no
    /// `tool_calls` (the model's final answer) or after
    /// `self.max_tool_rounds` rounds, whichever comes first.
    ///
    /// `response_format` is withheld from every round while tools are still
    /// in play and only attached to the final, tool-free round: many
    /// OpenAI-compatible servers, given a strict `json_schema` response
    /// format, force schema-conforming output and never emit `tool_calls` at
    /// all, which would silently stop tools from ever firing. See
    /// `docs/usage/ja/mcp.md`.
    ///
    /// `self.skills` (resolved against `env.skill_cache`, `lait.config.yml`'s
    /// top-level `skills:`) is appended to `system_prompt` before either path
    /// below ever sees it — see `with_skills`. `active_agent_paths` is every
    /// subagent file currently executing on this call stack (canonicalized);
    /// pass `&[]` for a top-level call (chat/`lait agent run`/a workflow
    /// step) and `call_subagent_tool` extends it for a subagent's own
    /// completion, so a subagent chain that cycles back to itself is caught
    /// the same way `workflow:` nesting is (see `MAX_SUBAGENT_DEPTH`).
    /// `turn.history`/`turn.image_urls` are only ever non-empty for chat's
    /// own call site (a resumed `--session`, `--image`); every other caller
    /// passes `PromptTurn { history: &[], image_urls: &[], .. }`, which
    /// reproduces the exact message shape this method built before either
    /// feature existed — see `llm::initial_messages`.
    pub(crate) async fn complete(
        &self,
        env: &AppContext,
        active_agent_paths: &[PathBuf],
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<response::ChatCompletionResponse> {
        let system_prompt = self
            .system_prompt_with_skills(&env.skill_cache, turn.system_prompt, cancellation.clone())
            .await?;
        let system_prompt = system_prompt.as_deref();

        if self.mcp.is_empty() && self.subagents.is_empty() {
            let messages =
                llm::initial_messages(system_prompt, turn.history, turn.prompt, turn.image_urls)?;
            return self
                .complete_recorded(env, response_format, messages, &[], cancellation)
                .await;
        }

        // `agent_registry.tools` is synchronous (it only reads local subagent
        // files), so joining it with the MCP round trip lets both proceed
        // together instead of paying the MCP latency before ever touching
        // disk.
        let (mut mcp_tool_set, mut subagent_tool_set) = tokio::try_join!(
            env.registry.tools(&self.mcp, cancellation.clone()),
            env.agent_registry
                .tools_cancellable(&self.subagents, cancellation.clone()),
        )?;
        for name in subagent_tool_set.names() {
            if mcp_tool_set.contains(name) {
                bail!("tool name collision: an MCP tool and a subagent both qualify to '{name}'");
            }
        }
        // Only `.contains()`/`.subagent_name()` (which read `.index`, not
        // `.tools`) are used below, so `.tools` doesn't need to survive past
        // this merge — moving it out avoids cloning every tool definition
        // (including its full JSON `parameters`).
        let mut tools = std::mem::take(&mut mcp_tool_set.tools);
        tools.extend(std::mem::take(&mut subagent_tool_set.tools));

        let mut messages =
            llm::initial_messages(system_prompt, turn.history, turn.prompt, turn.image_urls)?;

        let mut round = 0usize;
        loop {
            round += 1;
            if round > self.max_tool_rounds {
                bail!(
                    "tool loop exceeded max_tool_rounds ({}) without the model producing a final response",
                    self.max_tool_rounds
                );
            }

            let response = self
                .complete_recorded(env, None, messages.clone(), &tools, cancellation.clone())
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
                return self
                    .complete_recorded(env, response_format, messages, &[], cancellation.clone())
                    .await;
            };

            let content = response::first_message(&response).and_then(|message| message.content());
            messages.push(llm::assistant_tool_call_message(tool_calls, content)?);

            // A model turn's `tool_calls` are independent by construction (it
            // couldn't have seen one call's result before deciding on
            // another), so they're run concurrently rather than one at a
            // time; `try_join_all` preserves `tool_calls`' order regardless
            // of completion order, so the appended `tool`-role messages stay
            // in a stable, deterministic order.
            let tool_messages =
                futures_util::future::try_join_all(tool_calls.iter().map(|tool_call| async {
                    let name = &tool_call.function.name;
                    let result = if mcp_tool_set.contains(name) {
                        env.registry
                            .call(
                                &mcp_tool_set,
                                name,
                                &tool_call.function.arguments,
                                cancellation.clone(),
                            )
                            .await?
                    } else if let Some(subagent_name) = subagent_tool_set.subagent_name(name) {
                        call_subagent_tool(
                            subagent_name,
                            &tool_call.function.arguments,
                            env,
                            active_agent_paths,
                            cancellation.clone(),
                        )
                        .await?
                    } else {
                        bail!("model called unknown tool '{name}'");
                    };
                    llm::tool_result_message(&tool_call.id, result)
                }))
                .await?;
            messages.extend(tool_messages);
        }
    }

    /// The one way `complete` sends a request: builds it via `request`,
    /// awaits it, and records the response's usage under
    /// `self.usage_label` — so no future call site can forget the recording
    /// and skew `--show-usage`.
    async fn complete_recorded(
        &self,
        env: &AppContext,
        response_format: Option<ResponseFormat>,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: &[ChatCompletionTools],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<response::ChatCompletionResponse> {
        let response =
            llm::complete(self.request(response_format, messages, tools, cancellation)).await?;
        env.usage.record_response(&self.usage_label, &response);
        Ok(response)
    }

    /// Like [`RequestSettings::complete`], but requests a streamed response.
    /// Rejects `self.mcp`/`self.subagents` being non-empty: a streamed
    /// `tool_calls` field arrives as index-keyed fragments that must be
    /// reassembled before they can be routed to an MCP server or a subagent,
    /// which lait does not yet do (see `docs/usage/ja/mcp.md`). `self.skills`
    /// is appended to `system_prompt` the same way as in `complete` — skills
    /// are static injection, so unlike `mcp`/`subagents` they impose no such
    /// restriction on streaming.
    /// `include_usage` asks the server for a final usage chunk (see
    /// `llm::CompletionRequest::stream_include_usage`); set it only when the
    /// caller will actually display it (`--show-usage`). `turn.history`/
    /// `turn.image_urls` behave exactly as in `complete` — see its doc comment.
    pub(crate) async fn complete_stream(
        &self,
        skill_cache: &skill::SkillCache,
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        include_usage: bool,
    ) -> Result<llm::CompletionStream> {
        if !self.mcp.is_empty() {
            bail!(
                "'--stream'/streaming is not supported together with 'mcp:' yet; drop one of them"
            );
        }
        if !self.subagents.is_empty() {
            bail!(
                "'--stream'/streaming is not supported together with 'subagents:' yet; drop one \
                 of them"
            );
        }
        let system_prompt = self
            .system_prompt_with_skills(skill_cache, turn.system_prompt, None)
            .await?;
        let messages = llm::initial_messages(
            system_prompt.as_deref(),
            turn.history,
            turn.prompt,
            turn.image_urls,
        )?;
        let mut request = self.request(response_format, messages, &[], None);
        request.stream_include_usage = include_usage;
        llm::complete_stream(request).await
    }

    /// Shared by `complete`/`complete_stream`: resolves `self.skills` against
    /// `skill_cache` and appends the result to `system_prompt` — see
    /// `with_skills`.
    async fn system_prompt_with_skills<'a>(
        &self,
        skill_cache: &skill::SkillCache,
        system_prompt: Option<&'a str>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Option<Cow<'a, str>>> {
        let skills_text = skill_cache.render(&self.skills, cancellation).await?;
        Ok(with_skills(system_prompt, skills_text.as_deref()))
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
/// Returns the accumulated content text (for `--session`/`lait history` to
/// record — see `StreamOutcome`) alongside the usage carried by the final
/// chunk, when the request asked for one (see
/// `RequestSettings::complete_stream`'s `include_usage`) and the server
/// obliged. `output_path` redirects the content to a file (`-o`): the file
/// then holds the body alone, so reasoning deltas — normally written ahead of
/// the content on stdout — go to stderr instead.
pub(crate) async fn stream_response(
    mut stream: llm::CompletionStream,
    show_reasoning: bool,
    output_path: Option<&Path>,
) -> Result<StreamOutcome> {
    use std::io::Write;

    // Locked/opened once for the stream's whole lifetime: every delta write
    // below would otherwise re-acquire stdout's mutex, and nothing else
    // prints to the content sink while a response is streaming.
    let mut stdout_lock;
    let mut file_writer;
    // Whether reasoning shares the content sink (the stdout presentation:
    // a `Reasoning:` header, then a blank line before the content).
    let reasoning_inline = output_path.is_none();
    let content_sink: &mut dyn Write = match output_path {
        None => {
            stdout_lock = std::io::stdout().lock();
            &mut stdout_lock
        }
        Some(path) => {
            let file = std::fs::File::create(path)
                .with_context(|| format!("failed to create output file '{}'", path.display()))?;
            file_writer = std::io::BufWriter::new(file);
            &mut file_writer
        }
    };
    let mut wrote_reasoning = false;
    let mut wrote_content = false;
    let mut last_usage = None;
    let mut content_text = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(usage) = chunk.usage {
            last_usage = Some(usage);
        }
        let (content, reasoning) = response::stream_chunk_deltas(&chunk);
        if show_reasoning && let Some(reasoning) = reasoning {
            if reasoning_inline {
                if !wrote_reasoning {
                    writeln!(content_sink, "Reasoning:")?;
                }
                write!(content_sink, "{reasoning}")?;
                content_sink.flush()?;
            } else {
                if !wrote_reasoning {
                    eprintln!("Reasoning:");
                }
                eprint!("{reasoning}");
            }
            wrote_reasoning = true;
        }
        if let Some(content) = content {
            if reasoning_inline && wrote_reasoning && !wrote_content {
                write!(content_sink, "\n\n")?;
            }
            write!(content_sink, "{content}")?;
            // Only the live stdout display needs each delta pushed out
            // immediately; a `-o` file's `BufWriter` batches until the final
            // flush below instead of paying a syscall per delta.
            if reasoning_inline {
                content_sink.flush()?;
            }
            wrote_content = true;
            content_text.push_str(content);
        }
    }

    if !wrote_content {
        bail!("API response contained no content in its first choice");
    }
    if !reasoning_inline && wrote_reasoning {
        eprintln!();
    }
    writeln!(content_sink)?;
    content_sink.flush()?;
    Ok(StreamOutcome {
        content: content_text,
        usage: last_usage,
    })
}

/// What `stream_response` produced: the full response text (concatenated
/// from every content delta, exactly as printed) and the usage its final
/// chunk carried, if any. The content half exists for callers that need the
/// complete text after the stream ends even though it was already written
/// out incrementally — recording a `--session` turn or a `lait history`
/// entry, neither of which can work from deltas alone.
pub(crate) struct StreamOutcome {
    pub(crate) content: String,
    pub(crate) usage: Option<response::Usage>,
}

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
    env: &AppContext,
    turn: AgentTurn<'_>,
    steps_outputs: &workflow::StepOutputs,
    active_agent_paths: &[PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let system_prompt = template::render(
        &agent_file.system_prompt_template,
        turn.input,
        steps_outputs,
        &serde_json::Map::new(),
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
    response::render_response(&response, false, false)
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

/// Runs subagent `name` (resolved via `env.agent_registry`, an `agents:`
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
    env: &'a AppContext,
    active_paths: &'a [PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        // `Copy` (it only captures `name: &str`), so it can back every
        // `with_context` call below without re-typing the same `format!`.
        let context = || format!("subagent '{name}'");

        let loaded = env
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

        let settings = agent_file_settings(&loaded.file, &env.file_config, Some(name))
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

/// Resolves the settings for one completion request. `model_name` and every
/// field of `overrides` must already reflect the caller's own precedence
/// chain (e.g. step > agent > workflow default); this only adds the two
/// layers every caller shares: the resolved model's own defaults, then
/// `lait.config.yml`'s `default:` block. `local_models` is the alias map to
/// check before falling back to `file_config`'s (a workflow's embedded
/// `models:`, or empty when there is none). `capability_overrides`
/// (`mcp`/`max_tool_rounds`/`skills`) follows the same two-layer fallback
/// (caller's own value, then `file_config.default`) — there is no
/// per-model-alias equivalent, unlike `reasoning_effort`/`temperature`, since
/// neither an MCP server nor a skill has a natural connection to a model
/// definition.
pub(crate) fn resolve_request_settings(
    model_name: String,
    overrides: SamplingOverrides,
    base_url_override: Option<String>,
    api_key_override: Option<String>,
    capability_overrides: CapabilityOverrides,
    local_models: &ModelMap,
    file_config: &ConfigFile,
) -> Result<RequestSettings> {
    let resolved_model = match config::resolve_model_alias(&model_name, local_models)? {
        Some(resolved) => resolved,
        None => config::resolve_model(model_name, file_config)?,
    };
    let (base_url, api_key) = config::resolve_endpoint(
        base_url_override,
        api_key_override,
        resolved_model.base_url.as_deref(),
        resolved_model.api_key.as_deref(),
        file_config,
    )?;
    let api_key = api_key.unwrap_or_else(|| {
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
    let request_context = format!("the request for model '{}'", resolved_model.model_id);
    llm::validate_sampling_params(
        sampling.temperature,
        sampling.top_p,
        sampling.max_tokens,
        &request_context,
    )?;

    let mcp = capability_overrides
        .mcp
        .or_else(|| file_config.default.mcp.clone())
        .unwrap_or_default();
    let max_tool_rounds = capability_overrides
        .max_tool_rounds
        .or(file_config.default.max_tool_rounds);
    llm::validate_max_tool_rounds(max_tool_rounds, &request_context)?;
    let max_tool_rounds = max_tool_rounds.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);
    let skills = capability_overrides
        .skills
        .or_else(|| file_config.default.skills.clone())
        .unwrap_or_default();
    let subagents = capability_overrides
        .subagents
        .or_else(|| file_config.default.subagents.clone())
        .unwrap_or_default();

    Ok(RequestSettings {
        base_url,
        api_key,
        resolved_model,
        sampling,
        mcp,
        max_tool_rounds,
        skills,
        subagents,
        usage_label: String::new(),
    })
}

/// Resolves an agent file's own `RequestSettings` — its `model` (required,
/// falling back to `default.model`), sampling overrides, and
/// `mcp`/`max_tool_rounds`/`skills`/`subagents` — against `file_config`.
/// Shared by `run_agent` (the top-level `lait agent run` entry point) and
/// `call_subagent_tool` (a subagent invoked as a tool mid-completion), which
/// both need exactly this: an agent file's *own* settings, independent of
/// any caller/step context (unlike `resolve_step_settings`, which layers a
/// workflow node's own overrides on top). `subagent_name` names what a
/// missing-model error is about — `None` for the top-level agent, or
/// `Some("x")` for subagent `x` — built into the error message only if it's
/// actually needed, so `call_subagent_tool` doesn't pay for a `format!` on
/// every subagent call just for a message that's read on the rare
/// missing-model path.
pub(crate) fn agent_file_settings(
    agent_file: &AgentFile,
    file_config: &ConfigFile,
    subagent_name: Option<&str>,
) -> Result<RequestSettings> {
    let model_name = agent_file
        .model
        .clone()
        .or_else(|| file_config.default.model.clone())
        .ok_or_else(|| {
            let subject = subagent_name
                .map(|name| format!(" for subagent '{name}'"))
                .unwrap_or_default();
            anyhow!(
                "model is required{subject}; set it in its frontmatter or default.model in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
    resolve_request_settings(
        model_name,
        SamplingOverrides {
            reasoning_effort: agent_file.reasoning_effort,
            temperature: agent_file.temperature,
            top_p: agent_file.top_p,
            max_tokens: agent_file.max_tokens,
        },
        None,
        None,
        CapabilityOverrides {
            mcp: agent_file.mcp.clone(),
            max_tool_rounds: agent_file.max_tool_rounds,
            skills: agent_file.skills.clone(),
            subagents: agent_file.subagents.clone(),
        },
        &ModelMap::default(),
        file_config,
    )
}
