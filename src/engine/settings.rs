//! Resolving a completion request's settings and endpoint/fallback
//! candidates: [`resolve_request_settings`]/[`agent_file_settings`] (the two
//! public entry points every caller — chat, `lait prompt`, `lait agent run`,
//! a workflow node — goes through to build a [`RequestSettings`]), plus the
//! fallback machinery ([`is_fallback_eligible`], [`EndpointAttempt`],
//! `RequestSettings::advance_to_next_candidate`) that `complete_recorded`/
//! `stream_endpoint` in the parent module drive their retry loop with.
//! `RequestSettings` itself, and the rest of its `impl` block (`request`,
//! `complete`, `complete_stream`), stay in the parent module — they're the
//! request/response pipeline this module only feeds settings into.

use anyhow::{Result, anyhow};
use async_openai::error::OpenAIError;

use crate::{
    agent::AgentFile,
    config::{self, ConfigFile, ModelMap},
    llm,
};

use super::{
    CapabilityOverrides, DEFAULT_MAX_TOOL_ROUNDS, RequestSettings, ResolvedCapabilities,
    RunContext, SamplingOverrides,
};

/// A `--base-url`/`--api-key` override, bundled together because every
/// caller that sets one always sources both from the same place
/// (`SharedChatArgs::endpoint`, for chat's own CLI flags) and every other
/// caller — resolving a model alias's or agent file's own endpoint instead —
/// omits both. See `resolve_request_settings`'s doc comment for why setting
/// either collapses `fallback_candidates` to a single entry.
#[derive(Debug, Default, Clone)]
pub(crate) struct EndpointOverrides {
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
}

/// Classifies whether `error` is worth falling back from, for
/// `RequestSettings::complete_recorded`/`complete_stream`'s
/// `advance_to_next_candidate`: a connection failure/timeout, or an API
/// response with a 5xx/429/408 status — the same retryable set
/// async-openai's own `OpenAIRetryLayer` already retries within a single
/// candidate (see `llm::client`'s doc comment), just at the level of
/// switching to a different `models:` definition instead of the same one
/// again. Anything else (a 4xx request/auth error, a malformed response,
/// lait's own cancellation/timeout errors, which are plain `anyhow!`
/// strings rather than an `OpenAIError` at all) fails the whole request
/// immediately — falling back on those would silently paper over what's
/// very likely the caller's own mistake (a bad request body, wrong
/// credentials) rather than the transient/capacity problem fallback exists
/// for.
pub(super) fn is_fallback_eligible(error: &anyhow::Error) -> bool {
    let Some(openai_error) = error.downcast_ref::<OpenAIError>() else {
        return false;
    };
    match openai_error {
        OpenAIError::ApiError(response) => {
            let status = response.status_code.as_u16();
            status >= 500 || status == 429 || status == 408
        }
        OpenAIError::Reqwest(reqwest_error) => {
            reqwest_error.is_connect() || reqwest_error.is_timeout()
        }
        _ => false,
    }
}

/// The one candidate `RequestSettings::complete_recorded`/`complete_stream`
/// are currently attempting — the primary endpoint at first, then whichever
/// `FallbackCandidate` `advance_to_next_candidate` last resolved. Bundled
/// into one struct purely so `RequestSettings::request` stays under
/// clippy's `too_many_arguments` threshold.
pub(super) struct EndpointAttempt {
    pub(super) base_url: String,
    pub(super) api_key: config::ApiKeySource,
    pub(super) model_id: String,
}

impl EndpointAttempt {
    pub(super) fn primary(settings: &RequestSettings) -> Self {
        Self {
            base_url: settings.base_url.clone(),
            api_key: settings.api_key.clone(),
            model_id: settings.resolved_model.model_id.clone(),
        }
    }
}

impl RequestSettings {
    /// Advances `(base_url, api_key_source, model_id)` past a failed attempt
    /// to the next `self.fallback_candidates` entry, selecting that
    /// candidate's endpoint data but leaving any `api_key_cmd` inert. Returns
    /// `Ok(false)` (leaving the three unchanged) once
    /// `candidates` is exhausted, telling the caller to give up and return
    /// its original error instead. Shared by `complete_recorded`/
    /// `complete_stream`'s otherwise-identical fallback loops — see
    /// `is_fallback_eligible` for what actually triggers a call to this.
    pub(super) fn advance_to_next_candidate(
        &self,
        env: &RunContext,
        candidates: &mut std::slice::Iter<'_, config::FallbackCandidate>,
        endpoint: &mut EndpointAttempt,
        error: &anyhow::Error,
    ) -> Result<bool> {
        let Some(candidate) = candidates.next() else {
            return Ok(false);
        };
        eprintln!(
            "warning: request to {} failed ({error:#}); falling back to model definition's \
             next entry ('{}')",
            endpoint.base_url, candidate.model_id
        );
        tracing::warn!(
            failed_base_url = %endpoint.base_url,
            next_model_id = %candidate.model_id,
            error = %error,
            "falling back to the next model definition entry",
        );
        let next_endpoint =
            config::resolve_fallback_endpoint(candidate, &env.services.file_config)?;
        endpoint.base_url = next_endpoint.base_url;
        endpoint.api_key = next_endpoint.api_key;
        endpoint.model_id = candidate.model_id.clone();
        Ok(true)
    }
}

fn api_key_source_name(source: &config::ApiKeySource) -> &'static str {
    match source {
        config::ApiKeySource::Absent => "absent",
        config::ApiKeySource::Literal(_) => "literal",
        config::ApiKeySource::Command(_) => "command",
    }
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
    endpoint_overrides: EndpointOverrides,
    capability_overrides: CapabilityOverrides,
    local_models: &ModelMap,
    file_config: &ConfigFile,
) -> Result<RequestSettings> {
    let EndpointOverrides {
        base_url: base_url_override,
        api_key: api_key_override,
    } = endpoint_overrides;

    // A `--base-url`/`--api-key` override pins every attempt to the same
    // endpoint regardless of which model-definition entry it came from, so
    // fallback candidates (each with their own `base_url`) would be
    // meaningless — collapse to a single candidate (see
    // `docs/usage/ja/config.md`'s フォールバック section). Otherwise, the
    // candidates come from whichever map `model_name` actually resolved
    // against below (`local_models`, e.g. a workflow's embedded `models:`,
    // takes precedence over `file_config.models` the same way
    // `resolve_model_alias`/`resolve_model` do).
    let fallback_candidates = if base_url_override.is_some() || api_key_override.is_some() {
        Vec::new()
    } else if local_models.contains_key(&model_name) {
        config::resolve_model_fallbacks(&model_name, local_models)?
    } else {
        config::resolve_model_fallbacks(&model_name, &file_config.models)?
    };

    let resolved_model = match config::resolve_model_alias(&model_name, local_models)? {
        Some(resolved) => resolved,
        None => config::resolve_model(model_name, file_config)?,
    };
    let endpoint = config::resolve_endpoint(
        base_url_override,
        api_key_override,
        Some(&resolved_model),
        file_config,
    )?;
    let config::Endpoint { base_url, api_key } = endpoint;
    let sampling = overrides.resolve(&resolved_model, &file_config.default);
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

    let ResolvedCapabilities {
        mcp,
        max_tool_rounds,
        skills,
        subagents,
        tools,
    } = capability_overrides.resolve(&file_config.default);
    llm::validate_max_tool_rounds(max_tool_rounds, &request_context)?;
    let max_tool_rounds = max_tool_rounds.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);

    tracing::debug!(
        model_id = %resolved_model.model_id,
        base_url = %base_url,
        api_key_source = %api_key_source_name(&api_key),
        reasoning_effort = ?sampling.reasoning_effort,
        temperature = ?sampling.temperature,
        top_p = ?sampling.top_p,
        max_tokens = ?sampling.max_tokens,
        mcp = ?mcp,
        max_tool_rounds,
        skills = ?skills,
        subagents = ?subagents,
        tools = ?tools,
        "resolved request settings",
    );

    Ok(RequestSettings {
        base_url,
        api_key,
        resolved_model,
        fallback_candidates,
        sampling,
        mcp,
        max_tool_rounds,
        skills,
        subagents,
        tools,
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
        EndpointOverrides::default(),
        CapabilityOverrides {
            mcp: agent_file.mcp.clone(),
            max_tool_rounds: agent_file.max_tool_rounds,
            skills: agent_file.skills.clone(),
            subagents: agent_file.subagents.clone(),
            tools: agent_file.tools.clone(),
        },
        &ModelMap::default(),
        file_config,
    )
}

#[cfg(test)]
mod tests {
    use super::is_fallback_eligible;
    use async_openai::error::{ApiError, ApiErrorResponse, OpenAIError};

    fn api_error(status: u16) -> anyhow::Error {
        anyhow::Error::from(OpenAIError::ApiError(ApiErrorResponse {
            status_code: reqwest::StatusCode::from_u16(status).unwrap(),
            api_error: ApiError {
                message: "boom".to_owned(),
                r#type: None,
                param: None,
                code: None,
            },
        }))
    }

    #[test]
    fn falls_back_on_server_errors() {
        assert!(is_fallback_eligible(&api_error(500)));
        assert!(is_fallback_eligible(&api_error(503)));
    }

    #[test]
    fn falls_back_on_rate_limit_and_request_timeout() {
        assert!(is_fallback_eligible(&api_error(429)));
        assert!(is_fallback_eligible(&api_error(408)));
    }

    #[test]
    fn does_not_fall_back_on_a_client_error() {
        assert!(!is_fallback_eligible(&api_error(400)));
        assert!(!is_fallback_eligible(&api_error(401)));
        assert!(!is_fallback_eligible(&api_error(404)));
    }

    #[test]
    fn does_not_fall_back_on_a_plain_anyhow_error() {
        // lait's own cancellation/timeout errors are plain `anyhow!` strings,
        // not an `OpenAIError` — see `is_fallback_eligible`'s doc comment on
        // why falling back on those would be wrong.
        assert!(!is_fallback_eligible(&anyhow::anyhow!(
            "operation was cancelled"
        )));
    }
}
