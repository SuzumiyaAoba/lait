use std::{collections::HashMap, fs};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::cli::ReasoningEffort;

pub(crate) const CONFIG_FILE_NAME: &str = "lait.config.yml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigFile {
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    #[serde(default)]
    pub(crate) default: DefaultSettings,
    #[serde(default)]
    models: ModelMap,
}

/// The `default:` block shared by `lait.config.yml` and a workflow file: a
/// fallback model/reasoning effort used when a step (or, for the config file,
/// the CLI/env) doesn't specify its own.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DefaultSettings {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

/// A map of model alias to its candidate definitions, as used by both
/// `lait.config.yml`'s top-level `models:` and a workflow file's `models:`.
pub(crate) type ModelMap = HashMap<String, Vec<ModelDefinition>>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelDefinition {
    provider: ProviderConfig,
    model_id: String,
    default_reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfig {
    base_url: String,
    api_key: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ResolvedModel {
    pub(crate) model_id: String,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

/// Resolves `model_name` against a single alias map, returning `Ok(None)` when the
/// map has no entry for it (as opposed to the map having an invalid entry, which
/// is an error).
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
    if definition.model_id.trim().is_empty() {
        bail!("model_id in model definition {model_name:?} must not be empty");
    }

    Ok(Some(ResolvedModel {
        model_id: definition.model_id.clone(),
        base_url: Some(definition.provider.base_url.clone()),
        api_key: definition.provider.api_key.clone(),
        reasoning_effort: definition.default_reasoning_effort,
    }))
}

pub(crate) fn resolve_model(model_name: String, config: &ConfigFile) -> Result<ResolvedModel> {
    if let Some(resolved) = resolve_model_alias(&model_name, &config.models)? {
        return Ok(resolved);
    }
    Ok(ResolvedModel {
        model_id: model_name,
        base_url: None,
        api_key: None,
        reasoning_effort: None,
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

pub(crate) fn load_config(no_config: bool) -> Result<ConfigFile> {
    if no_config {
        return Ok(ConfigFile::default());
    }

    let path = std::env::current_dir()
        .context("failed to determine the current directory for configuration")?
        .join(CONFIG_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigFile::default());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read YAML configuration file '{}'",
                    path.display()
                )
            });
        }
    };

    serde_yaml::from_str(&contents).with_context(|| {
        format!(
            "failed to parse YAML configuration file '{}'",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::expand_with;
    use std::collections::HashMap;

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
