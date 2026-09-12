//! Model alias/fallback resolution, endpoint selection, and `${VAR}`
//! environment-variable expansion. `resolve_endpoint`/`resolve_fallback_endpoint`
//! and the `expand_*` helpers are entangled (an endpoint's base URL/API key
//! layers are each expanded only after the layer wins — see
//! `resolve_endpoint`'s own doc comment) — kept in one file rather than the
//! design plan's earlier `config/expand.rs` split, which would have cut
//! `resolve_endpoint` in half.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};

use super::types::{
    ApiKeySource, CommandSpec, ConfigFile, Endpoint, ModelMap, ResolvedModel, check_api_key_source,
};

pub(crate) fn resolve_model_alias(
    model_name: &str,
    models: &ModelMap,
) -> Result<Option<ResolvedModel>> {
    let Some(definitions) = models.get(model_name) else {
        return Ok(None);
    };
    let definition = definitions.first().ok_or_else(|| {
        anyhow!("model definition {model_name:?} must contain at least one entry")
    })?;
    let context = format!("model definition {model_name:?}");
    definition.validate(&context)?;

    Ok(Some(definition.resolved_model()))
}

/// One `models:` alias definition beyond the first (which
/// `resolve_model_alias`/`ResolvedModel` already covers) — the endpoint
/// `RequestSettings::complete_recorded`/`complete_stream` falls back to when
/// an earlier candidate fails with a retryable error. Unlike `ResolvedModel`,
/// this carries no sampling defaults: `docs/usage/ja/config.md`'s "複数
/// プロバイダーによるフォールバック" section documents that a fallback
/// candidate's `default_reasoning_effort`/`default_temperature`/etc. are
/// never used — only the first (primary) definition's sampling defaults
/// apply, regardless of which candidate the request actually lands on.
#[derive(Debug, Clone)]
pub(crate) struct FallbackCandidate {
    pub(crate) model_id: String,
    pub(crate) base_url: String,
    pub(crate) api_key: Option<String>,
    pub(crate) api_key_cmd: Option<CommandSpec>,
}

/// Resolves every `models:` alias definition after the first into a
/// [`FallbackCandidate`] list, in order — empty when `model_name` isn't an
/// alias in `models`, or the alias has only one definition. Validated the
/// same way `resolve_model_alias` validates the first definition (empty
/// `model_id`, `api_key`+`api_key_cmd` both set), so a broken fallback entry
/// is caught even on a run where the primary candidate always succeeds and
/// the broken one is never actually attempted.
pub(crate) fn resolve_model_fallbacks(
    model_name: &str,
    models: &ModelMap,
) -> Result<Vec<FallbackCandidate>> {
    let Some(definitions) = models.get(model_name) else {
        return Ok(Vec::new());
    };
    definitions
        .iter()
        .skip(1)
        .map(|definition| {
            let context = format!("model definition {model_name:?}");
            definition.validate(&context)?;
            Ok(definition.fallback_candidate())
        })
        .collect()
}

/// Resolves `candidate`'s endpoint (`${VAR}`-expanded, trailing slash
/// trimmed — the same normalization `resolve_endpoint` applies to the
/// primary candidate) and selects its API-key source (literal,
/// `api_key_cmd`, or falling back to the top-level `api_key`/`api_key_cmd`).
/// Only called when a candidate is about to be attempted. It never launches
/// an `api_key_cmd`; the returned [`ApiKeySource::Command`] is resolved by the
/// shared asynchronous secret resolver at the actual request boundary.
pub(crate) fn resolve_fallback_endpoint(
    candidate: &FallbackCandidate,
    file_config: &ConfigFile,
) -> Result<Endpoint> {
    let base_url = normalize_base_url(expand_env_placeholders(&candidate.base_url)?)?;
    check_api_key_source(
        &file_config.api_key,
        &file_config.api_key_cmd,
        "top-level configuration",
    )?;
    let api_key = select_api_key_source(
        None,
        candidate.api_key.as_deref(),
        candidate.api_key_cmd.as_ref(),
        file_config.api_key.as_deref(),
        file_config.api_key_cmd.as_ref(),
    )?;
    Ok(Endpoint { base_url, api_key })
}

/// Selects one API-key source in precedence order. Literal values from config
/// are expanded here, after their layer has won; command specs remain inert
/// data for [`crate::secret::SecretResolver`] to execute asynchronously at
/// request time.
fn select_api_key_source(
    override_value: Option<String>,
    api_key: Option<&str>,
    api_key_cmd: Option<&CommandSpec>,
    config_api_key: Option<&str>,
    config_api_key_cmd: Option<&CommandSpec>,
) -> Result<ApiKeySource> {
    if let Some(api_key) = override_value {
        return Ok(ApiKeySource::Literal(api_key));
    }
    if let Some(api_key) = api_key {
        return Ok(ApiKeySource::Literal(expand_env_placeholders(api_key)?));
    }
    if let Some(command) = api_key_cmd {
        return Ok(ApiKeySource::Command(command.clone()));
    }
    if let Some(api_key) = config_api_key {
        return Ok(ApiKeySource::Literal(expand_env_placeholders(api_key)?));
    }
    if let Some(command) = config_api_key_cmd {
        return Ok(ApiKeySource::Command(command.clone()));
    }
    Ok(ApiKeySource::Absent)
}

pub(super) fn expand_list(values: &[String]) -> Result<Vec<String>> {
    values
        .iter()
        .map(|value| expand_env_placeholders(value))
        .collect()
}

pub(super) fn expand_map(values: &HashMap<String, String>) -> Result<HashMap<String, String>> {
    values
        .iter()
        .map(|(key, value)| Ok((key.clone(), expand_env_placeholders(value)?)))
        .collect()
}

fn normalize_base_url(base_url: String) -> Result<String> {
    let base_url = base_url.trim_end_matches('/').to_owned();
    if base_url.is_empty() {
        bail!("base URL must not be empty");
    }
    Ok(base_url)
}

pub(crate) fn resolve_model(model_name: String, config: &ConfigFile) -> Result<ResolvedModel> {
    // Catches an empty/whitespace `model:` from any layer (an agent file's
    // frontmatter, a workflow's `default.model`, a node's own `model:`) that
    // would otherwise pass straight through as an empty `model` request
    // field; the chat entry point filters empty names out before ever
    // resolving, but the file-sourced layers have no other check.
    if model_name.trim().is_empty() {
        bail!("model name must not be empty");
    }
    if let Some(resolved) = resolve_model_alias(&model_name, &config.models)? {
        return Ok(resolved);
    }
    Ok(ResolvedModel {
        model_id: model_name,
        base_url: None,
        api_key: None,
        api_key_cmd: None,
        reasoning_effort: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
    })
}

/// Expands every `${VAR_NAME}` placeholder in `value` by substituting the
/// named environment variable, so a config/workflow file can reference a
/// secret (e.g. an API key) without writing it in plaintext. Applied only to
/// `base_url`/`api_key` values sourced from `lait.config.yml` or a workflow's
/// embedded `models:`/top-level settings — never to a `--base-url`/`--api-key`
/// CLI override, which the shell already expands on its own. Errors if a
/// placeholder's variable is unset; a value with no `${...}` is returned
/// unchanged.
pub(crate) fn expand_env_placeholders(value: &str) -> Result<String> {
    expand_with(value, |name| std::env::var(name).ok())
}

const DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";

/// Resolves the endpoint a request goes to from the three layers every
/// caller shares — explicit override > model-definition value > config
/// top-level — falling back to `DEFAULT_BASE_URL`, normalizing the trailing
/// slash, and rejecting an empty base URL. `${VAR}` placeholders are only
/// expanded in the config-sourced layers (see `expand_env_placeholders`),
/// never in an override, which the shell already expands on its own.
///
/// The API key follows the same three layers, except each of the two
/// config-sourced ones (`model_api_key`/`model_api_key_cmd`,
/// `file_config.api_key`/`file_config.api_key_cmd`) may set a literal value
/// *or* an `api_key_cmd` — never both (`check_api_key_source` rejects that
/// regardless of which layer ends up winning). The selected command is kept
/// inert in the returned [`Endpoint`] and is resolved by the asynchronous
/// secret resolver only when a request is sent. `ApiKeySource::Absent` means
/// `RequestSettings` can use its dummy key for async-openai, while
/// `lait models --remote` can omit the Authorization header.
pub(crate) fn resolve_endpoint(
    base_url_override: Option<String>,
    api_key_override: Option<String>,
    resolved_model: Option<&ResolvedModel>,
    file_config: &ConfigFile,
) -> Result<Endpoint> {
    let model_base_url = resolved_model.and_then(|model| model.base_url.as_deref());
    let model_api_key = resolved_model.and_then(|model| model.api_key.as_deref());
    let model_api_key_cmd = resolved_model.and_then(|model| model.api_key_cmd.as_ref());

    // Select the source before expanding it. Apart from avoiding needless
    // work, this is important for precedence: an unset `${VAR}` in a lower
    // priority source must not make a request fail when an override already
    // supplies the endpoint that will be used.
    let base_url = match base_url_override {
        Some(base_url) => base_url,
        None => match model_base_url {
            Some(base_url) => expand_env_placeholders(base_url)?,
            None => file_config
                .base_url
                .as_deref()
                .map(expand_env_placeholders)
                .transpose()?
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
        },
    };
    let base_url = normalize_base_url(base_url)?;

    check_api_key_source(
        &file_config.api_key,
        &file_config.api_key_cmd,
        "top-level configuration",
    )?;
    let api_key = select_api_key_source(
        api_key_override,
        model_api_key,
        model_api_key_cmd,
        file_config.api_key.as_deref(),
        file_config.api_key_cmd.as_ref(),
    )?;
    Ok(Endpoint { base_url, api_key })
}

/// The parsing logic behind `expand_env_placeholders`, taking a `lookup`
/// function instead of reading `std::env` directly so it can be unit tested
/// without touching real process environment variables (mutating those from
/// Rust's threaded test runner is both racy and, as of edition 2024, `unsafe`).
fn expand_with(value: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        let after_brace = &rest[start + 2..];
        let Some(end_offset) = after_brace.find('}') else {
            bail!("unterminated '${{' placeholder in {value:?}");
        };
        let var_name = &after_brace[..end_offset];
        if var_name.is_empty()
            || !var_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!(
                "invalid environment variable placeholder '${{{var_name}}}' in {value:?} (must be alphanumeric/underscore)"
            );
        }
        let var_value = lookup(var_name).ok_or_else(|| {
            anyhow!("environment variable '{var_name}' referenced by '${{{var_name}}}' is not set")
        })?;
        result.push_str(&var_value);
        rest = &after_brace[end_offset + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{expand_with, normalize_base_url};

    #[test]
    fn normalize_base_url_removes_trailing_slashes() {
        assert_eq!(
            normalize_base_url("https://example.com///".to_owned()).unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn normalize_base_url_rejects_an_empty_value() {
        assert!(normalize_base_url("///".to_owned()).is_err());
    }

    fn lookup_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn returns_a_value_with_no_placeholder_unchanged() {
        assert_eq!(
            expand_with("plain-value", lookup_from(&[])).unwrap(),
            "plain-value"
        );
    }

    #[test]
    fn expands_a_whole_string_placeholder() {
        assert_eq!(
            expand_with("${API_KEY}", lookup_from(&[("API_KEY", "secret")])).unwrap(),
            "secret"
        );
    }

    #[test]
    fn expands_a_placeholder_embedded_in_a_larger_string() {
        assert_eq!(
            expand_with(
                "https://${HOST}/v1",
                lookup_from(&[("HOST", "api.example.com")])
            )
            .unwrap(),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn expands_multiple_placeholders() {
        assert_eq!(
            expand_with(
                "${SCHEME}://${HOST}",
                lookup_from(&[("SCHEME", "https"), ("HOST", "example.com")])
            )
            .unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn errors_when_the_referenced_variable_is_unset() {
        let error = expand_with("${MISSING}", lookup_from(&[])).unwrap_err();
        assert!(error.to_string().contains("MISSING"));
    }

    #[test]
    fn errors_on_an_unterminated_placeholder() {
        assert!(expand_with("${UNCLOSED", lookup_from(&[])).is_err());
    }

    #[test]
    fn errors_on_an_empty_placeholder_name() {
        assert!(expand_with("${}", lookup_from(&[])).is_err());
    }

    #[test]
    fn errors_on_a_placeholder_name_with_invalid_characters() {
        assert!(expand_with("${API-KEY}", lookup_from(&[("API-KEY", "x")])).is_err());
    }
}
