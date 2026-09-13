//! The LLM request/response layer: resolving a completion request's
//! settings, sending it (including the MCP/subagent tool-call loop), and
//! calling an agent file's own template/schema/completion pipeline. Shared by
//! every caller that ultimately talks to a model — chat, `lait prompt`,
//! `lait agent run`, and a workflow node's `agent`/`prompt` action.
//!
//! Interactive tool-call approval (the `--approve-tools` stderr prompt and
//! its process-wide serialization gate) lives in [`approval`] rather than
//! here, since it is terminal UI wedged into this layer rather than part of
//! the request/response pipeline itself; `tool_loop` calls into it for every
//! tool call before dispatching.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use crate::{
    cache, cassette, config, llm, mcp, reasoning::ReasoningEffort, response, shell_tool, skill,
    subagent,
};
use anyhow::{Result, bail};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat,
};

mod agent;
mod approval;
mod context;
mod settings;
mod stream;
mod tool_loop;

pub(crate) use agent::{AgentTurn, call_agent, call_subagent_tool, value_to_input_text};
pub(crate) use context::{AppServices, RunContext};
use settings::{EndpointAttempt, is_fallback_eligible};
pub(crate) use settings::{EndpointOverrides, agent_file_settings, resolve_request_settings};
use stream::{StreamOutcome, stream_response};
use tool_loop::ToolLoop;

/// The maximum number of tool-call round trips a single completion request
/// may take (see `RequestSettings::complete`) before lait gives up and
/// errors instead of looping forever on a model that keeps calling tools.
/// Overridable per CLI invocation/agent file/workflow node/`default:` via
/// `max_tool_rounds`.
const DEFAULT_MAX_TOOL_ROUNDS: usize = 8;

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

    /// `resolve_request_settings`'s tail of the fallback chain [`fold`] does
    /// not cover: `resolved_model`'s own defaults, then
    /// `lait.config.yml`'s `default:` block — each field falling back
    /// independently, same as `fold`.
    ///
    /// [`fold`]: Self::fold
    pub(crate) fn resolve(
        self,
        resolved_model: &config::ResolvedModel,
        default: &config::DefaultSettings,
    ) -> Self {
        Self {
            reasoning_effort: self
                .reasoning_effort
                .or(resolved_model.reasoning_effort)
                .or(default.reasoning_effort),
            temperature: self
                .temperature
                .or(resolved_model.temperature)
                .or(default.temperature),
            top_p: self.top_p.or(resolved_model.top_p).or(default.top_p),
            max_tokens: self
                .max_tokens
                .or(resolved_model.max_tokens)
                .or(default.max_tokens),
        }
    }
}

/// The `mcp`/`max_tool_rounds`/`skills`/`subagents`/`tools` knobs a caller
/// may set for a single completion request, bundled the same way as
/// `SamplingOverrides` and for the same reason (keeps
/// `resolve_request_settings`'s argument count down; each field falls back
/// independently to `file_config.default`, not as a whole unit).
#[derive(Debug, Default, Clone)]
pub(crate) struct CapabilityOverrides {
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Option<Vec<String>>,
    pub(crate) subagents: Option<Vec<String>>,
    /// Names of `tools:` entries (see `config::ShellToolDefinition`) made
    /// available as callable shell-command tools during this request's tool
    /// loop. Falls back independently, like `mcp`.
    pub(crate) tools: Option<Vec<String>>,
}

impl CapabilityOverrides {
    /// Folds `layers` field by field in priority order — see
    /// `SamplingOverrides::fold`, which this mirrors, except `layers` is
    /// taken by value: unlike `SamplingOverrides`' `Copy` fields, each field
    /// here is a `Vec<String>` the caller already owns (built fresh per
    /// call, one per layer). A borrowed `&[Self]` would need its own
    /// `.clone()` to move a winning field out of a `&Self` — on top of the
    /// clone the caller already pays constructing its owned `Self` layers —
    /// so every field would be cloned twice. Taking ownership here instead
    /// lets each field move out of whichever layer supplies it, exactly
    /// once, in one pass over `layers`.
    pub(crate) fn fold<const N: usize>(layers: [Self; N]) -> Self {
        let mut folded = Self::default();
        for layer in layers {
            folded.mcp = folded.mcp.or(layer.mcp);
            folded.max_tool_rounds = folded.max_tool_rounds.or(layer.max_tool_rounds);
            folded.skills = folded.skills.or(layer.skills);
            folded.subagents = folded.subagents.or(layer.subagents);
            folded.tools = folded.tools.or(layer.tools);
        }
        folded
    }

    /// `resolve_request_settings`'s tail of the fallback chain [`fold`] does
    /// not cover: `lait.config.yml`'s `default:` block, with every
    /// list-valued field defaulted to empty rather than left `None` — unlike
    /// `max_tool_rounds`, which [`ResolvedCapabilities`] keeps as a raw
    /// `Option` since its caller still has to validate it before choosing
    /// [`DEFAULT_MAX_TOOL_ROUNDS`].
    ///
    /// [`fold`]: Self::fold
    pub(crate) fn resolve(self, default: &config::DefaultSettings) -> ResolvedCapabilities {
        ResolvedCapabilities {
            mcp: self.mcp.or_else(|| default.mcp.clone()).unwrap_or_default(),
            max_tool_rounds: self.max_tool_rounds.or(default.max_tool_rounds),
            skills: self
                .skills
                .or_else(|| default.skills.clone())
                .unwrap_or_default(),
            subagents: self
                .subagents
                .or_else(|| default.subagents.clone())
                .unwrap_or_default(),
            tools: self
                .tools
                .or_else(|| default.tools.clone())
                .unwrap_or_default(),
        }
    }
}

/// [`CapabilityOverrides::resolve`]'s result — every field already merged
/// with `lait.config.yml`'s `default:` block, except `max_tool_rounds`,
/// which stays a raw `Option<usize>` because its caller must validate it
/// (`llm::validate_max_tool_rounds`) before substituting
/// [`DEFAULT_MAX_TOOL_ROUNDS`].
pub(crate) struct ResolvedCapabilities {
    pub(crate) mcp: Vec<String>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Vec<String>,
    pub(crate) subagents: Vec<String>,
    pub(crate) tools: Vec<String>,
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

/// The model/base-URL/API-key-source/sampling settings for a single completion
/// request, after resolving aliases and applying every fallback layer. An
/// API-key command remains inert until the request boundary.
pub(crate) struct RequestSettings {
    pub(crate) base_url: String,
    /// The selected key source is kept inert until a request is sent. This
    /// keeps settings resolution safe for dry-run/lint and avoids running a
    /// secrets command for a request that is served from replay/cache.
    pub(crate) api_key: config::ApiKeySource,
    pub(crate) resolved_model: config::ResolvedModel,
    /// Further `models:` alias definitions to fall back to, in order, when
    /// the primary endpoint above fails with a retryable error (a
    /// connection failure/timeout, or a 5xx/429/408 response) — see
    /// `complete_recorded`/`complete_stream`'s shared `attempt_with_fallback`
    /// and `docs/usage/ja/config.md`'s フォールバック section. Empty when
    /// `model_name` wasn't resolved from a `models:` alias, or a
    /// `--base-url`/`--api-key` override collapsed every candidate into one
    /// (see `resolve_request_settings`).
    pub(crate) fallback_candidates: Vec<config::FallbackCandidate>,
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
    /// Names of `tools:` entries made available as callable shell-command
    /// tools during this request's tool loop. Empty means "no shell tools" —
    /// combined with `mcp`/`subagents` the same way, and included in the
    /// same fast-path check. See `crate::shell_tool`.
    pub(crate) tools: Vec<String>,
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

/// Unwraps `complete_recorded`'s `content_key`, which it computes once up
/// front whenever `cache`/`--record`/`--replay` is active. Every call site
/// only runs inside a branch already gated on one of those three being
/// active, so `content_key` is provably `Some` there — this just gives the
/// three call sites one shared message to document that invariant, instead
/// of each repeating (and each needing to stay in sync with) its own
/// slightly different wording.
fn require_content_key(content_key: &Option<String>) -> &str {
    content_key
        .as_deref()
        .expect("content_key is computed above whenever cache/record/replay is active")
}

/// Checks that no qualified tool name is claimed by more than one of the
/// three tool sources a request can combine — `mcp::qualify_tool_name`
/// prefixes each source differently (`<server>__`/`agent__`/`tool__`), so a
/// collision only happens if two *different* servers/agents/shell tools
/// happen to render to the same sanitized name (or a `tools:` entry is
/// literally named the same as an `agents:` entry, etc.). Shared by
/// `complete`/`complete_stream`, which both assemble the same three sets.
fn check_tool_name_collisions(
    mcp_tool_set: &mcp::ToolSet,
    subagent_tool_set: &subagent::ToolSet,
    shell_tool_set: &shell_tool::ToolSet,
) -> Result<()> {
    for name in subagent_tool_set.names() {
        if mcp_tool_set.contains(name) {
            bail!("tool name collision: an MCP tool and a subagent both qualify to '{name}'");
        }
    }
    for name in shell_tool_set.names() {
        if mcp_tool_set.contains(name) {
            bail!("tool name collision: an MCP tool and a shell tool both qualify to '{name}'");
        }
        if subagent_tool_set.subagent_name(name).is_some() {
            bail!("tool name collision: a subagent and a shell tool both qualify to '{name}'");
        }
    }
    Ok(())
}

impl RequestSettings {
    /// Sets `usage_label` — see that field's doc comment.
    pub(crate) fn with_usage_label(mut self, label: impl Into<String>) -> Self {
        self.usage_label = label.into();
        self
    }

    /// Builds an `llm::CompletionRequest` from these settings' sampling
    /// parameters (the same for every request `self` ever builds, and never
    /// affected by which endpoint candidate is being attempted — see
    /// `FallbackCandidate`'s doc comment) plus `endpoint` (the candidate
    /// currently being attempted) and the per-call `response_format`/
    /// `messages`/`tools`. Both `complete`'s tool loop and `complete_stream`
    /// go through here instead of repeating this field list at each call site.
    fn request<'a>(
        &'a self,
        endpoint: &'a EndpointAttempt,
        api_key: &'a str,
        response_format: Option<ResponseFormat>,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: &'a [ChatCompletionTools],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> llm::CompletionRequest<'a> {
        llm::CompletionRequest {
            base_url: &endpoint.base_url,
            api_key,
            model_id: &endpoint.model_id,
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
    /// the model, and if it comes back with `tool_calls`, `env.services.registry`
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
    /// `self.skills` (resolved against `env.services.skill_cache`, `lait.config.yml`'s
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
        env: &RunContext,
        active_agent_paths: &[PathBuf],
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<response::ChatCompletionResponse> {
        let messages = self
            .initial_turn_messages(env, turn, cancellation.clone())
            .await?;

        if self.mcp.is_empty() && self.subagents.is_empty() && self.tools.is_empty() {
            return self
                .complete_recorded(env, response_format, &messages, &[], cancellation)
                .await;
        }

        let mut tool_loop = self
            .assemble_tool_loop(env, messages, cancellation.clone())
            .await?;
        loop {
            tool_loop.next_round(self.max_tool_rounds)?;

            let response = self
                .complete_recorded(
                    env,
                    None,
                    tool_loop.messages(),
                    tool_loop.tools(),
                    cancellation.clone(),
                )
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
                let messages = tool_loop.into_messages();
                return self
                    .complete_recorded(env, response_format, &messages, &[], cancellation.clone())
                    .await;
            };

            let content = response::first_message(&response).and_then(|message| message.content());
            tool_loop
                .append_tool_calls(
                    tool_calls,
                    content,
                    env,
                    active_agent_paths,
                    cancellation.clone(),
                )
                .await?;
        }
    }

    /// Builds the three tool sets (`mcp:`, `subagents:`, `tools:`) `complete`/
    /// `complete_stream` dispatch calls against, plus their merged OpenAI-
    /// shaped `tools:` payload — shared by both since streamed and
    /// non-streamed requests assemble tools identically, only how each round
    /// is issued differs. `agent_registry.tools`/`shell_tool::tools` are both
    /// synchronous (they only read local subagent files/`file_config.tools`),
    /// so joining the MCP round trip with the subagent one lets both proceed
    /// together instead of paying the MCP latency before ever touching disk;
    /// `shell_tool::tools` is cheap enough to just call inline after.
    ///
    /// Callers must not call this when `self.mcp`/`self.subagents`/
    /// `self.tools` are all empty — that's the plain-completion fast path,
    /// handled separately above.
    async fn assemble_tool_sets(
        &self,
        env: &RunContext,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<(
        mcp::ToolSet,
        subagent::ToolSet,
        shell_tool::ToolSet,
        Vec<ChatCompletionTools>,
    )> {
        let (mut mcp_tool_set, mut subagent_tool_set) = tokio::try_join!(
            env.services.registry.tools(&self.mcp, cancellation.clone()),
            env.services
                .agent_registry
                .tools_cancellable(&self.subagents, cancellation.clone()),
        )?;
        let mut shell_tool_set = shell_tool::tools(&self.tools, &env.services.file_config.tools)?;
        check_tool_name_collisions(&mcp_tool_set, &subagent_tool_set, &shell_tool_set)?;
        // Only `.contains()`/`.subagent_name()`/`.tool_name()` (which read
        // `.index`, not `.tools`) are used by callers below, so `.tools`
        // doesn't need to survive past this merge — moving it out avoids
        // cloning every tool definition (including its full JSON `parameters`).
        let mut tools = std::mem::take(&mut mcp_tool_set.tools);
        tools.extend(std::mem::take(&mut subagent_tool_set.tools));
        tools.extend(std::mem::take(&mut shell_tool_set.tools));
        Ok((mcp_tool_set, subagent_tool_set, shell_tool_set, tools))
    }

    /// Builds the stateful tool loop used by either completion transport.
    /// Keeping assembly and state construction together prevents the streamed
    /// and non-streamed paths from drifting apart as a new tool source is
    /// added.
    async fn assemble_tool_loop(
        &self,
        env: &RunContext,
        messages: Vec<ChatCompletionRequestMessage>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<ToolLoop> {
        let (mcp_tool_set, subagent_tool_set, shell_tool_set, tools) =
            self.assemble_tool_sets(env, cancellation.clone()).await?;
        Ok(ToolLoop::new(
            messages,
            mcp_tool_set,
            subagent_tool_set,
            shell_tool_set,
            tools,
        ))
    }

    /// The one way `complete` sends a request: checks the response disk
    /// cache first when `env.policy.cache.enabled()` (see `crate::cache` and
    /// `docs/usage/ja/config.md`'s キャッシュ section — a hit skips the
    /// network entirely and is *not* recorded in `--show-usage`, since no
    /// request was actually sent), otherwise builds the request via
    /// `request`, awaits it, records the response's usage under
    /// `self.usage_label` — so no future call site can forget the recording
    /// and skew `--show-usage` — and, still only on a cache-enabled miss,
    /// writes the response back to the cache. Tries `self.fallback_candidates`
    /// in order after a retryable failure (see `is_fallback_eligible`/
    /// `advance_to_next_candidate`) before giving up. The cache key is
    /// always computed from the *primary* endpoint (`self.base_url`/
    /// `self.resolved_model.model_id`), never whichever fallback candidate
    /// actually served the request — the cache represents "what would this
    /// logical request return", not which of possibly several endpoints
    /// happened to answer it.
    /// `--replay`'s lookup path: every request is answered from
    /// `replay_dir`'s cassettes, or the run fails outright (see
    /// `cassette::load`) — `None` when `--replay` isn't active, so
    /// `complete_recorded` falls through to its cache/network paths.
    async fn try_replay(
        &self,
        env: &RunContext,
        content_key: &Option<String>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Option<response::ChatCompletionResponse>> {
        let Some(replay_dir) = env.policy.cassette.replay_dir() else {
            return Ok(None);
        };
        let key = require_content_key(content_key);
        let response =
            cassette::load(replay_dir, key, &self.resolved_model.model_id, cancellation).await?;
        env.usage.record_response(&self.usage_label, &response);
        Ok(Some(response))
    }

    /// The response disk cache's read path — skipped entirely while
    /// `--record`ing (a cache hit would otherwise skip the network call
    /// `--record` needs to actually observe; the later cache *save* stays
    /// harmless there). A hit returns `Some` without recording usage (see
    /// `complete_recorded`'s doc comment: a cache hit is not a network
    /// request, so `--show-usage` must not count it); a miss or read failure
    /// (logged at `debug`, treated the same as a miss) returns `None` so the
    /// caller falls through to an actual request.
    async fn try_cache(
        &self,
        env: &RunContext,
        content_key: &Option<String>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Option<response::ChatCompletionResponse>> {
        if !(env.policy.cache.enabled() && env.policy.cassette.record_dir().is_none()) {
            return Ok(None);
        }
        let cache_key = require_content_key(content_key);
        match cache::load(
            cache_key,
            env.policy.cache.ttl(),
            chrono::Utc::now(),
            cancellation,
        )
        .await
        {
            Ok(Some(response)) => {
                eprintln!("note: cache hit for {}", self.usage_label);
                tracing::debug!(cache_key = %cache_key, "response cache hit");
                Ok(Some(response))
            }
            Ok(None) => {
                tracing::debug!(cache_key = %cache_key, "response cache miss");
                Ok(None)
            }
            Err(error) => {
                tracing::debug!(
                    cache_key = %cache_key,
                    error = %error,
                    "failed to read response cache entry; treating it as a miss",
                );
                Ok(None)
            }
        }
    }

    async fn complete_recorded(
        &self,
        env: &RunContext,
        response_format: Option<ResponseFormat>,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTools],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<response::ChatCompletionResponse> {
        // The same content hash serves three purposes below (response cache,
        // `--record` cassette filename, `--replay` cassette lookup) —
        // computed once whenever any of the three is in play, always from
        // the *primary* endpoint (see this method's doc comment on why the
        // cache key ignores which fallback candidate actually answers).
        let content_key = if env.policy.cache.enabled()
            || env.policy.cassette.record_dir().is_some()
            || env.policy.cassette.replay_dir().is_some()
        {
            Some(cache::key(
                &self.base_url,
                &self.resolved_model.model_id,
                self.sampling,
                messages,
                tools,
                response_format.as_ref(),
            )?)
        } else {
            None
        };

        if let Some(response) = self
            .try_replay(env, &content_key, cancellation.clone())
            .await?
        {
            return Ok(response);
        }
        if let Some(response) = self
            .try_cache(env, &content_key, cancellation.clone())
            .await?
        {
            return Ok(response);
        }

        let mut endpoint = EndpointAttempt::primary(self);
        let mut candidates = self.fallback_candidates.iter();
        loop {
            let api_key = env
                .services
                .secret_resolver
                .resolve(&endpoint.api_key, cancellation.clone())
                .await?
                .unwrap_or_else(|| "lm-studio".to_owned());
            let request = self.request(
                &endpoint,
                &api_key,
                response_format.clone(),
                messages.to_vec(),
                tools,
                cancellation.clone(),
            );
            match llm::complete(request).await {
                Ok(response) => {
                    env.usage.record_response(&self.usage_label, &response);
                    if env.policy.cache.enabled()
                        && let Some(cache_key) = &content_key
                        && let Err(error) = cache::save(
                            cache_key,
                            &response,
                            chrono::Utc::now(),
                            cancellation.clone(),
                        )
                        .await
                    {
                        tracing::debug!(error = %error, "failed to write response cache entry");
                    }
                    if let Some(record_dir) = env.policy.cassette.record_dir() {
                        let key = require_content_key(&content_key);
                        cassette::save(
                            record_dir,
                            key,
                            &endpoint.base_url,
                            &endpoint.model_id,
                            messages,
                            tools,
                            response_format.as_ref(),
                            &response,
                            cancellation.clone(),
                        )
                        .await?;
                    }
                    return Ok(response);
                }
                Err(error) if is_fallback_eligible(&error) => {
                    if !self.advance_to_next_candidate(
                        env,
                        &mut candidates,
                        &mut endpoint,
                        &error,
                    )? {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Sends one streamed request and returns the raw stream — the streaming
    /// counterpart of `complete_recorded`'s single-request-plus-fallback
    /// loop, minus the response disk cache (a stream is never cached; see
    /// `docs/usage/ja/config.md`'s キャッシュ section). `tools` lets a
    /// streamed round advertise MCP/subagent tools the same way a
    /// non-streamed round's `tools` slice does — `complete_stream` (below)
    /// is the only caller, and passes `&[]` for a request with no tool
    /// sources. Falls back through `self.fallback_candidates`, like
    /// `complete_recorded`, but only up to the point a candidate's
    /// `llm::complete_stream` call itself returns `Err` — once a stream is
    /// established (`Ok`), lait commits to it: a streamed `tool_calls`/
    /// content delta arriving from one candidate can't be silently resent to
    /// another mid-stream.
    async fn stream_endpoint(
        &self,
        env: &RunContext,
        response_format: Option<ResponseFormat>,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTools],
        include_usage: bool,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<llm::CompletionStream> {
        let mut endpoint = EndpointAttempt::primary(self);
        let mut candidates = self.fallback_candidates.iter();
        loop {
            let api_key = env
                .services
                .secret_resolver
                .resolve(&endpoint.api_key, cancellation.clone())
                .await?
                .unwrap_or_else(|| "lm-studio".to_owned());
            let mut request = self.request(
                &endpoint,
                &api_key,
                response_format.clone(),
                messages.to_vec(),
                tools,
                cancellation.clone(),
            );
            request.stream_include_usage = include_usage;
            match llm::complete_stream(request).await {
                Ok(stream) => return Ok(stream),
                Err(error) if is_fallback_eligible(&error) => {
                    if !self.advance_to_next_candidate(
                        env,
                        &mut candidates,
                        &mut endpoint,
                        &error,
                    )? {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Like [`RequestSettings::complete`], but streams each round's response
    /// to `output_path` (`None` for stdout) as it arrives instead of waiting
    /// for the full completion — driving the same MCP/subagent tool loop
    /// `complete` does when `self.mcp`/`self.subagents` names at least one
    /// tool source, reassembling each round's streamed `tool_calls`
    /// fragments (see `response::StreamToolCallAccumulator`) before handing
    /// them to the same `ToolLoop::append_tool_calls` dispatch `complete`'s
    /// non-streamed loop uses. `self.skills` is appended to `system_prompt`
    /// the same way as in `complete`. `include_usage` asks the server for a
    /// final usage chunk on every round (see
    /// `llm::CompletionRequest::stream_include_usage`); set it only when the
    /// caller will actually display it (`--show-usage`). `turn.history`/
    /// `turn.image_urls`/`active_agent_paths` behave exactly as in
    /// `complete` — see its doc comment. Returns the *last* round's
    /// [`StreamOutcome`] (the one whose content was actually the final
    /// answer) — an intermediate round's content, if any, was still streamed
    /// to `output_path` as it arrived, exactly like the final round's, since
    /// there is no way to know a round is not the last one until after it
    /// has already finished streaming.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_stream(
        &self,
        env: &RunContext,
        active_agent_paths: &[PathBuf],
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        include_usage: bool,
        show_reasoning: bool,
        output_path: Option<&Path>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<StreamOutcome> {
        let messages = self
            .initial_turn_messages(env, turn, cancellation.clone())
            .await?;

        if self.mcp.is_empty() && self.subagents.is_empty() && self.tools.is_empty() {
            let stream = self
                .stream_endpoint(
                    env,
                    response_format,
                    &messages,
                    &[],
                    include_usage,
                    cancellation.clone(),
                )
                .await?;
            return stream_response(stream, show_reasoning, output_path, false, cancellation).await;
        }

        let mut tool_loop = self
            .assemble_tool_loop(env, messages, cancellation.clone())
            .await?;
        loop {
            let round = tool_loop.next_round(self.max_tool_rounds)?;

            let stream = self
                .stream_endpoint(
                    env,
                    None,
                    tool_loop.messages(),
                    tool_loop.tools(),
                    include_usage,
                    cancellation.clone(),
                )
                .await?;
            // Every round after the first must append rather than truncate
            // `output_path` — otherwise a later round's `File::create` would
            // wipe out whatever an earlier round already streamed to it.
            let outcome = stream_response(
                stream,
                show_reasoning,
                output_path,
                round > 1,
                cancellation.clone(),
            )
            .await?;

            if outcome.tool_calls.is_empty() {
                if response_format.is_none() {
                    return Ok(outcome);
                }
                // The model stopped calling tools; re-issue the same history
                // once more with `response_format` attached, now that doing
                // so can no longer suppress a tool call — mirrors
                // `complete`'s own re-issue. This round's own outcome is
                // discarded in favor of the reissue's, so — unlike the
                // `return Ok(outcome)` above, whose usage the caller records
                // from the returned `StreamOutcome` — its usage has to be
                // recorded here or it's lost entirely; see the fallthrough
                // branch below for why every non-final round needs this.
                if let Some(usage) = outcome.usage {
                    env.usage.record(&self.usage_label, usage);
                }
                let messages = tool_loop.into_messages();
                let stream = self
                    .stream_endpoint(
                        env,
                        response_format,
                        &messages,
                        &[],
                        include_usage,
                        cancellation.clone(),
                    )
                    .await?;
                return stream_response(stream, show_reasoning, output_path, true, cancellation)
                    .await;
            }

            // Unlike `complete_recorded` (the non-streamed tool loop's
            // single choke point, which records every round's usage as it
            // happens), this round's `StreamOutcome` is consumed by
            // `ToolLoop::append_tool_calls` below and never reaches a caller — only
            // the loop's *final* round is ever returned, and that's the one
            // `app::run_chat`/`repl::run_turn` record from the returned
            // `StreamOutcome`. Recording here is this round's only chance to
            // be counted at all; skipping it (as this loop did before) would
            // silently undercount `--show-usage` by every tool-calling round
            // but the last.
            if let Some(usage) = outcome.usage {
                env.usage.record(&self.usage_label, usage);
            }

            let content = if outcome.content.is_empty() {
                None
            } else {
                Some(outcome.content.as_str())
            };
            tool_loop
                .append_tool_calls(
                    &outcome.tool_calls,
                    content,
                    env,
                    active_agent_paths,
                    cancellation.clone(),
                )
                .await?;
        }
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
        Ok(with_skills(
            system_prompt,
            skills_text.as_deref().map(String::as_str),
        ))
    }

    /// The prologue shared by `complete`/`complete_stream`: resolves the
    /// system prompt (with skills appended) and builds the initial message
    /// list from `turn`. Both callers used to run this exact computation
    /// twice each — once to build a tool-free fast path's messages, then
    /// again right after with identical arguments, since the fast path's
    /// early `return` made the two calls look unrelated even though nothing
    /// between them could change the result. Computing it once up front and
    /// branching *after* removes that redundant second call in both.
    async fn initial_turn_messages(
        &self,
        env: &RunContext,
        turn: PromptTurn<'_>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<ChatCompletionRequestMessage>> {
        let system_prompt = self
            .system_prompt_with_skills(&env.services.skill_cache, turn.system_prompt, cancellation)
            .await?;
        llm::initial_messages(
            system_prompt.as_deref(),
            turn.history,
            turn.prompt,
            turn.image_urls,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::context::{CachePolicy, CancellationSource, CassettePolicy, RunContext};
    use super::{AppServices, CapabilityOverrides, stream_response};
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use tokio_util::sync::CancellationToken;

    /// `CapabilityOverrides::fold` picks each field independently from the
    /// first layer (in priority order) that sets it, moving fields out
    /// instead of cloning them — this pins that behavior across a field the
    /// first layer sets, a field only a later layer sets, and a field no
    /// layer sets.
    #[test]
    fn capability_overrides_fold_picks_the_first_layer_that_sets_each_field_independently() {
        let highest_priority = CapabilityOverrides {
            mcp: Some(vec!["from-first".to_owned()]),
            ..Default::default()
        };
        let lower_priority = CapabilityOverrides {
            mcp: Some(vec!["from-second".to_owned()]),
            skills: Some(vec!["skill-a".to_owned()]),
            ..Default::default()
        };
        let lowest_priority = CapabilityOverrides {
            skills: Some(vec!["ignored".to_owned()]),
            tools: Some(vec!["tool-a".to_owned()]),
            ..Default::default()
        };

        let folded = CapabilityOverrides::fold([highest_priority, lower_priority, lowest_priority]);

        assert_eq!(folded.mcp, Some(vec!["from-first".to_owned()]));
        assert_eq!(folded.skills, Some(vec!["skill-a".to_owned()]));
        assert_eq!(folded.tools, Some(vec!["tool-a".to_owned()]));
        assert_eq!(folded.subagents, None);
        assert_eq!(folded.max_tool_rounds, None);
    }

    /// A regression test for the bug `stream_response`'s cancellation
    /// parameter fixes: before this, a stream that never produced another
    /// chunk (a server that accepted the connection but stopped responding
    /// mid-stream) had no way to be interrupted short of the server's own
    /// connection eventually dropping — nothing polled cancellation once
    /// `stream.next()` was already being awaited.
    #[tokio::test]
    async fn is_cancelled_promptly_instead_of_hanging_on_a_stream_that_never_completes() {
        let stream: crate::llm::CompletionStream = Box::pin(futures_util::stream::pending());
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            canceller.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            stream_response(stream, false, None, false, Some(cancellation)),
        )
        .await
        .expect("stream_response should return promptly once cancelled, not hang");

        let error = result.expect_err("a cancelled stream should be reported as an error");
        assert!(
            error.downcast_ref::<crate::error::Interrupted>().is_some(),
            "{error}"
        );
    }

    #[test]
    fn operation_cancellation_is_a_child_of_the_invocation_source() {
        let root = CancellationToken::new();
        let source = CancellationSource::new(root.clone());
        let operation = source.operation_token();

        operation.cancel();
        assert!(!root.is_cancelled());

        root.cancel();
        assert!(source.root_token().is_cancelled());
        assert!(operation.is_cancelled());
    }

    #[test]
    fn cassette_policy_rejects_record_and_replay_together() {
        let services = Arc::new(AppServices::new(Arc::new(
            crate::config::ConfigFile::default(),
        )));
        let context = RunContext::new(services, CancellationToken::new());
        let result = context
            .with_record_replay(Some(PathBuf::from("record")), Some(PathBuf::from("replay")));

        assert!(result.is_err());
    }

    #[test]
    fn cache_and_cassette_modes_expose_only_their_valid_operations() {
        let cache = CachePolicy::Enabled { ttl: Some(30) };
        assert!(cache.enabled());
        assert_eq!(cache.ttl(), Some(30));

        let record = CassettePolicy::Record(PathBuf::from("record"));
        assert!(record.record_dir().is_some());
        assert!(record.replay_dir().is_none());

        let replay = CassettePolicy::Replay(PathBuf::from("replay"));
        assert!(replay.record_dir().is_none());
        assert!(replay.replay_dir().is_some());
    }
}
