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

use crate::config;

mod agent;
mod approval;
mod context;
mod settings;
mod stream;
mod tool_loop;
mod transport;

pub(crate) use agent::{AgentTurn, call_agent, call_subagent_tool, value_to_input_text};
pub(crate) use context::{AppServices, RunContext};
pub(crate) use settings::{EndpointOverrides, agent_file_settings, resolve_request_settings};
pub(crate) use transport::StreamOptions;

// Defined in the crate-root `overrides` module (not a submodule of this one)
// so `cache::key` can depend on `SamplingOverrides` without depending on all
// of `engine` — see `overrides`'s module doc for why that would otherwise be
// a `{engine, cache}` dependency cycle. Re-exported here so every existing
// `engine::SamplingOverrides`-style call site elsewhere in the crate keeps
// working unchanged.
pub(crate) use crate::overrides::{
    CapabilityOverrides, PromptTurn, ResolvedCapabilities, SamplingOverrides,
};

/// The maximum number of tool-call round trips a single completion request
/// may take (see `RequestSettings::complete`) before lait gives up and
/// errors instead of looping forever on a model that keeps calling tools.
/// Overridable per CLI invocation/agent file/workflow node/`default:` via
/// `max_tool_rounds`.
const DEFAULT_MAX_TOOL_ROUNDS: usize = 8;

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
    /// `transport::complete_recorded`/`transport::stream_endpoint`, each of
    /// which drives its own fallback loop over this list (an `async`
    /// closure-based shared loop was tried and reverted — see that commit's
    /// message for why), and `docs/usage/ja/config.md`'s フォールバック
    /// section. Empty when `model_name` wasn't resolved from a `models:`
    /// alias, or a `--base-url`/`--api-key` override collapsed every
    /// candidate into one (see `resolve_request_settings`).
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

impl RequestSettings {
    /// Sets `usage_label` — see that field's doc comment.
    pub(crate) fn with_usage_label(mut self, label: impl Into<String>) -> Self {
        self.usage_label = label.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::context::{CachePolicy, CancellationSource, CassettePolicy, RunContext};
    use super::stream::stream_response;
    use super::{AppServices, CapabilityOverrides};
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
        assert!(crate::error::is_interrupted(&error), "{error}");
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
