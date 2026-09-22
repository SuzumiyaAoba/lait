//! Sending a completion request built by `settings::resolve_request_settings`/
//! `agent_file_settings`: the non-streamed (`RequestSettings::complete`) and
//! streamed (`RequestSettings::complete_stream`) tool-call loops, each
//! driving its own single-request-plus-fallback transport
//! (`complete_recorded`/`stream_endpoint`) through `self.fallback_candidates`.
//! Split out of the parent module — `settings` only builds a
//! [`super::RequestSettings`], it never sends one; this is the request/
//! response pipeline that actually does, so it gets its own file even though
//! `RequestSettings` itself (and its trivial `with_usage_label` setter) stay
//! in the parent module, per `impl RequestSettings`'s existing split there
//! and in `settings.rs` (`advance_to_next_candidate`).
//!
//! `complete_recorded` and `stream_endpoint` share an identical skeleton —
//! resolve the current candidate's API key, build a request, send it, and on
//! a fallback-eligible error advance to the next candidate via
//! `advance_to_next_candidate` before retrying — differing only in which
//! `llm::` function they call and, for `complete_recorded`, its success-arm
//! cache/cassette saves. P8-6a planned collapsing this into one
//! `with_fallback`-style helper and it was never done; P9 re-planned it and
//! **attempted and reverted it**, so the reason belongs here rather than
//! being re-discovered a third time.
//!
//! The two loops cannot be unified with a plain `Fn(CompletionRequest<'_>)
//! -> F where F: Future<Output = Result<T>>` closure parameter: the request
//! each call builds borrows from that call's own `endpoint`/`api_key`
//! locals, a fresh lifetime every loop iteration, so the future `send`
//! returns must vary per call in a way one associated type `F` cannot
//! express — a "lifetime may not live long enough" error. `AsyncFn`
//! (stable since Rust 1.85, which resolves exactly this case via an
//! internal GAT) compiles the helper itself, but introducing that bound
//! into this file breaks unrelated code elsewhere in the crate:
//! `workflow/exec.rs`'s and `workflow/exec/retry.rs`'s recursive
//! `Box::pin(async move { .. })` futures (`run_steps`'s own recursion, and
//! `execute_step_with_retry`'s) fail `Send` inference with "implementation
//! of `Send` is not general enough" for `&str`/`&WorkflowScope`/
//! `&FlowStep`/`&RouterContext<'_>` — a known class of rustc trait-solver
//! limitation where a higher-ranked closure bound in one part of a crate
//! can poison auto-trait inference for an unrelated recursive boxed future
//! elsewhere in the same crate. Verified by bisect: `cargo check
//! --locked --all-targets` is clean on the parent commit and fails with
//! these errors only once the `AsyncFn`-based `with_fallback` is added,
//! with no other change. A `Pin<Box<dyn Future<Output = Result<T>> + '_>>`
//! return type was also tried, on the theory that erasing the future's
//! concrete type would sidestep the associated-type problem without
//! `AsyncFn` — it still introduces the same higher-ranked bound the
//! `Send`-inference failure traces back to, and reproduces the identical
//! errors. The ~25 lines of duplication between the two loops are
//! therefore left as they are; each is already documented as the other's
//! counterpart (see `stream_endpoint`'s own doc comment).

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    cache, cassette, config, llm, mcp, report, response, shell_tool, skill, subagent, trace,
};

use super::{
    PromptTurn, RequestSettings, RunContext,
    settings::{EndpointAttempt, is_fallback_eligible},
    stream::{StreamOutcome, stream_response},
    tool_loop::ToolLoop,
};

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
/// four tool sources a request can combine — `mcp::qualify_tool_name`
/// prefixes each source differently (`<server>__`/`agent__`/`tool__`/
/// `skill__`), so a collision only happens if two *different*
/// servers/agents/shell tools/skills happen to render to the same sanitized
/// name (or a `tools:` entry is literally named the same as an `agents:`
/// entry, etc.). Shared by `complete`/`complete_stream`, which both assemble
/// the same four sets — `skill_tool_set` is empty unless
/// `default.skill_progressive_disclosure` is `true` (see
/// `assemble_tool_sets`), so this is a no-op extra loop in the common case.
fn check_tool_name_collisions(
    mcp_tool_set: &mcp::ToolSet,
    subagent_tool_set: &subagent::ToolSet,
    shell_tool_set: &shell_tool::ToolSet,
    skill_tool_set: &skill::ToolSet,
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
    for name in skill_tool_set.names() {
        if mcp_tool_set.contains(name) {
            bail!("tool name collision: an MCP tool and a skill both qualify to '{name}'");
        }
        if subagent_tool_set.subagent_name(name).is_some() {
            bail!("tool name collision: a subagent and a skill both qualify to '{name}'");
        }
        if shell_tool_set.tool_name(name).is_some() {
            bail!("tool name collision: a shell tool and a skill both qualify to '{name}'");
        }
    }
    Ok(())
}

/// The user-turn `RequestSettings::compact_tool_loop` appends to a tool
/// loop's own message history before asking the model to summarize it — see
/// `config::CompactionConfig`'s doc comment.
const COMPACTION_INSTRUCTION: &str = "Summarize our conversation so far concisely: the overall \
     goal, what has been tried, what was learned, and what (if anything) remains to be done. \
     This summary will replace the detailed history above in the ongoing conversation, so \
     include everything needed to continue effectively.";

/// The streaming-only options `complete_stream` needs beyond what `complete`
/// already takes (`response_format`/`turn`/`cancellation`/...) — grouped so
/// the method stays within clippy's argument-count lint without losing any
/// of `include_usage`/`show_reasoning`/`output_path`'s independent meaning.
pub(crate) struct StreamOptions<'a> {
    pub(crate) include_usage: bool,
    pub(crate) show_reasoning: bool,
    pub(crate) output_path: Option<&'a Path>,
}

impl RequestSettings {
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
        cancellation: CancellationToken,
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
        cancellation: CancellationToken,
    ) -> Result<response::ChatCompletionResponse> {
        let messages = self
            .initial_turn_messages(env, turn, cancellation.clone())
            .await?;

        if self.mcp.is_empty()
            && self.subagents.is_empty()
            && self.tools.is_empty()
            && !self.skills_need_tool_loop(env)
        {
            return self
                .complete_recorded(env, response_format, &messages, &[], cancellation)
                .await;
        }

        let mut tool_loop = self
            .assemble_tool_loop(env, messages, cancellation.clone())
            .await?;
        loop {
            tool_loop.next_round(self.max_tool_rounds)?;

            if let Some(compaction) = &env.services.file_config.default.compaction {
                self.maybe_compact(&mut tool_loop, compaction, env, cancellation.clone())
                    .await?;
            }

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

    /// Whether `self.skills` needs the tool loop entered even though
    /// `self.mcp`/`self.subagents`/`self.tools` are all empty: only true when
    /// `default.skill_progressive_disclosure` is `true` and `self.skills`
    /// actually names at least one skill, since only then does a
    /// `skill__<name>` tool exist for the model to call. With progressive
    /// disclosure off (the default), `self.skills` is still resolved into
    /// the system prompt by `system_prompt_with_skills`, but never needs a
    /// tool round trip — see `docs/usage/ja/skills.md`.
    fn skills_need_tool_loop(&self, env: &RunContext) -> bool {
        env.services
            .file_config
            .default
            .skill_progressive_disclosure
            == Some(true)
            && !self.skills.is_empty()
    }

    /// Builds the four tool sets (`mcp:`, `subagents:`, `tools:`, and —
    /// only when `default.skill_progressive_disclosure` is `true` —
    /// `skills:`) `complete`/`complete_stream` dispatch calls against, plus
    /// their merged OpenAI-shaped `tools:` payload — shared by both since
    /// streamed and non-streamed requests assemble tools identically, only
    /// how each round is issued differs. `agent_registry.tools`/
    /// `shell_tool::tools`/`skill::tools` are all synchronous (they only
    /// read local subagent files/`file_config.tools`/`file_config.skills`),
    /// so joining the MCP round trip with the subagent one lets both proceed
    /// together instead of paying the MCP latency before ever touching disk;
    /// `shell_tool::tools`/`skill::tools` are cheap enough to just call
    /// inline after.
    ///
    /// Callers must not call this when `self.mcp`/`self.subagents`/
    /// `self.tools` are all empty and `self.skills_need_tool_loop(env)` is
    /// `false` — that's the plain-completion fast path, handled separately
    /// above.
    async fn assemble_tool_sets(
        &self,
        env: &RunContext,
        cancellation: CancellationToken,
    ) -> Result<(
        mcp::ToolSet,
        subagent::ToolSet,
        shell_tool::ToolSet,
        skill::ToolSet,
        Vec<ChatCompletionTools>,
    )> {
        let (mut mcp_tool_set, mut subagent_tool_set) = tokio::try_join!(
            env.services.registry.tools(&self.mcp, cancellation.clone()),
            env.services
                .agent_registry
                .tools_cancellable(&self.subagents, cancellation.clone()),
        )?;
        let mut shell_tool_set = shell_tool::tools(&self.tools, &env.services.file_config.tools)?;
        let progressive_skill_names: &[String] = if env
            .services
            .file_config
            .default
            .skill_progressive_disclosure
            == Some(true)
        {
            &self.skills
        } else {
            &[]
        };
        let mut skill_tool_set =
            skill::tools(progressive_skill_names, &env.services.file_config.skills)?;
        check_tool_name_collisions(
            &mcp_tool_set,
            &subagent_tool_set,
            &shell_tool_set,
            &skill_tool_set,
        )?;
        // Only `.contains()`/`.subagent_name()`/`.tool_name()` (which read
        // `.index`, not `.tools`) are used by callers below, so `.tools`
        // doesn't need to survive past this merge — moving it out avoids
        // cloning every tool definition (including its full JSON `parameters`).
        let mut tools = std::mem::take(&mut mcp_tool_set.tools);
        tools.extend(std::mem::take(&mut subagent_tool_set.tools));
        tools.extend(std::mem::take(&mut shell_tool_set.tools));
        tools.extend(std::mem::take(&mut skill_tool_set.tools));
        Ok((
            mcp_tool_set,
            subagent_tool_set,
            shell_tool_set,
            skill_tool_set,
            tools,
        ))
    }

    /// Builds the stateful tool loop used by either completion transport.
    /// Keeping assembly and state construction together prevents the streamed
    /// and non-streamed paths from drifting apart as a new tool source is
    /// added.
    async fn assemble_tool_loop(
        &self,
        env: &RunContext,
        messages: Vec<ChatCompletionRequestMessage>,
        cancellation: CancellationToken,
    ) -> Result<ToolLoop> {
        let (mcp_tool_set, subagent_tool_set, shell_tool_set, skill_tool_set, tools) =
            self.assemble_tool_sets(env, cancellation.clone()).await?;
        Ok(ToolLoop::new(
            messages,
            mcp_tool_set,
            subagent_tool_set,
            shell_tool_set,
            skill_tool_set,
            tools,
            self.usage_label.clone(),
        ))
    }

    /// Compacts `tool_loop` when its about-to-be-sent round is a
    /// `compaction.trigger_rounds` multiple — see `config::CompactionConfig`'s
    /// doc comment for why, and [`compact_tool_loop`] for how. Only `complete`
    /// calls this (not `complete_stream`): a workflow `prompt`/`agent` node —
    /// the case this is chiefly aimed at, a long `mcp`/`subagents`/`tools`
    /// round-trip loop — always goes through `complete`, never streaming; see
    /// docs/usage/ja/compaction.md for this and `--trace-file`'s matching
    /// scope note.
    ///
    /// [`compact_tool_loop`]: Self::compact_tool_loop
    async fn maybe_compact(
        &self,
        tool_loop: &mut ToolLoop,
        compaction: &config::CompactionConfig,
        env: &RunContext,
        cancellation: CancellationToken,
    ) -> Result<()> {
        if compaction.trigger_rounds == 0 {
            bail!("default.compaction.trigger_rounds must be at least 1");
        }
        if !tool_loop.round().is_multiple_of(compaction.trigger_rounds) {
            return Ok(());
        }
        self.compact_tool_loop(tool_loop, compaction, env, cancellation)
            .await
    }

    /// Shrinks `tool_loop`'s growing message history by asking the model to
    /// summarize everything so far, then replacing all but the most recent
    /// `compaction.keep_last_n` messages with that summary (see
    /// `ToolLoop::splice_compacted`). The summarization request itself goes
    /// through `complete_recorded` — the same single choke point every other
    /// request in this file does — so it participates in `--cache`/
    /// `--record`/`--replay` and is recorded into `env.usage`/`env.trace`
    /// exactly like an ordinary round: a `--replay` run needs a cassette for
    /// it too, the same way it needs one for every round the original
    /// recording made. Also records its own `"compact"` trace event
    /// (distinct from the `"chat"` event `complete_recorded` already records
    /// for the summarization call itself), so a `--trace-file` reader can
    /// tell a compaction happened without inferring it from message content.
    async fn compact_tool_loop(
        &self,
        tool_loop: &mut ToolLoop,
        compaction: &config::CompactionConfig,
        env: &RunContext,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let start = chrono::Utc::now();
        let messages_before = tool_loop.messages().len();

        let mut request_messages = tool_loop.messages().to_vec();
        request_messages.push(llm::user_message(COMPACTION_INSTRUCTION, &[])?);
        let response = self
            .complete_recorded(env, None, &request_messages, &[], cancellation)
            .await?;
        let summary = response::content_text(&response);
        let summary_message = llm::assistant_message(&format!(
            "(summary of the conversation so far, produced by compaction: {summary})"
        ))?;

        tool_loop.splice_compacted(summary_message, compaction.keep_last_n);

        env.trace.record(
            "compact",
            self.usage_label.clone(),
            start,
            chrono::Utc::now(),
            trace::attrs([
                (
                    "lait.compaction.round",
                    Value::from(tool_loop.round() as u64),
                ),
                (
                    "lait.compaction.messages_before",
                    Value::from(messages_before as u64),
                ),
                (
                    "lait.compaction.messages_after",
                    Value::from(tool_loop.messages().len() as u64),
                ),
            ]),
        );
        Ok(())
    }

    /// Records a `"chat"`-operation [`trace::TraceEvent`] under
    /// `env.trace` — see `crate::trace`'s doc comment for why this is a
    /// flat, label-keyed event log rather than a span tree. `source`
    /// distinguishes a `--replay`/`--cache` hit from an actual network round
    /// trip (`"replay"`/`"cache"`/`"live"`); all three share the same
    /// `"chat"` operation name (OTel's `gen_ai.operation.name` vocabulary).
    /// Called from every one of `complete_recorded`'s three return paths
    /// (`try_replay`/`try_cache`/the network loop's success arm) so a
    /// `--trace-file` reader can tell which of those actually answered a
    /// given request, the same way `report::note`'s "cache hit" line
    /// already does for a human reading stderr.
    fn record_chat_trace(
        &self,
        env: &RunContext,
        source: &'static str,
        start: chrono::DateTime<chrono::Utc>,
        usage: Option<response::Usage>,
    ) {
        let mut attributes = trace::attrs([
            (
                "gen_ai.request.model",
                Value::from(self.resolved_model.model_id.clone()),
            ),
            ("lait.source", Value::from(source)),
        ]);
        if let Some(usage) = usage {
            attributes.insert(
                "gen_ai.usage.input_tokens".to_owned(),
                Value::from(usage.prompt_tokens),
            );
            attributes.insert(
                "gen_ai.usage.output_tokens".to_owned(),
                Value::from(usage.completion_tokens),
            );
        }
        env.trace.record(
            "chat",
            self.usage_label.clone(),
            start,
            chrono::Utc::now(),
            attributes,
        );
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
        cancellation: CancellationToken,
    ) -> Result<Option<response::ChatCompletionResponse>> {
        let Some(replay_dir) = env.policy.cassette.replay_dir() else {
            return Ok(None);
        };
        let key = require_content_key(content_key);
        let start = chrono::Utc::now();
        let response =
            cassette::load(replay_dir, key, &self.resolved_model.model_id, cancellation).await?;
        env.usage
            .record_response(&self.usage_label, &response, self.resolved_model.pricing);
        self.record_chat_trace(env, "replay", start, response.usage);
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
        cancellation: CancellationToken,
    ) -> Result<Option<response::ChatCompletionResponse>> {
        if !(env.policy.cache.enabled() && env.policy.cassette.record_dir().is_none()) {
            return Ok(None);
        }
        let cache_key = require_content_key(content_key);
        let start = chrono::Utc::now();
        match cache::load(cache_key, env.policy.cache.ttl(), start, cancellation).await {
            Ok(Some(response)) => {
                report::note(format_args!("cache hit for {}", self.usage_label));
                tracing::debug!(cache_key = %cache_key, "response cache hit");
                self.record_chat_trace(env, "cache", start, response.usage);
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
        cancellation: CancellationToken,
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
            let attempt_start = chrono::Utc::now();
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
                    env.usage.record_response(
                        &self.usage_label,
                        &response,
                        self.resolved_model.pricing,
                    );
                    self.record_chat_trace(env, "live", attempt_start, response.usage);
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
                            cassette::CassetteRequestRef {
                                base_url: &endpoint.base_url,
                                model_id: &endpoint.model_id,
                                messages,
                                tools,
                                response_format: response_format.as_ref(),
                            },
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
        cancellation: CancellationToken,
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
    /// to `stream.output_path` as it arrived, exactly like the final round's,
    /// since there is no way to know a round is not the last one until after
    /// it has already finished streaming.
    pub(crate) async fn complete_stream(
        &self,
        env: &RunContext,
        active_agent_paths: &[PathBuf],
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        stream: StreamOptions<'_>,
        cancellation: CancellationToken,
    ) -> Result<StreamOutcome> {
        let StreamOptions {
            include_usage,
            show_reasoning,
            output_path,
        } = stream;
        let messages = self
            .initial_turn_messages(env, turn, cancellation.clone())
            .await?;

        if self.mcp.is_empty()
            && self.subagents.is_empty()
            && self.tools.is_empty()
            && !self.skills_need_tool_loop(env)
        {
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
                    env.usage
                        .record(&self.usage_label, usage, self.resolved_model.pricing);
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
                env.usage
                    .record(&self.usage_label, usage, self.resolved_model.pricing);
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
    /// `env.services.skill_cache` and appends the result to `system_prompt`
    /// — see `with_skills`. Renders full skill bodies (`SkillCache::render`)
    /// unless `default.skill_progressive_disclosure` is `true`, in which case
    /// only each skill's frontmatter is appended
    /// (`SkillCache::render_frontmatter`) and the body is instead read on
    /// demand through a `skill__<name>` tool — see `skills_need_tool_loop`.
    async fn system_prompt_with_skills<'a>(
        &self,
        env: &RunContext,
        system_prompt: Option<&'a str>,
        cancellation: CancellationToken,
    ) -> Result<Option<Cow<'a, str>>> {
        let skill_cache = &env.services.skill_cache;
        let skills_text = if env
            .services
            .file_config
            .default
            .skill_progressive_disclosure
            == Some(true)
        {
            skill_cache
                .render_frontmatter(&self.skills, cancellation)
                .await?
        } else {
            skill_cache.render(&self.skills, cancellation).await?
        };
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
        cancellation: CancellationToken,
    ) -> Result<Vec<ChatCompletionRequestMessage>> {
        let system_prompt = self
            .system_prompt_with_skills(env, turn.system_prompt, cancellation)
            .await?;
        llm::initial_messages(
            system_prompt.as_deref(),
            turn.history,
            turn.prompt,
            turn.image_urls,
        )
    }
}
