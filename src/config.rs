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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelDefinition {
    provider: ProviderConfig,
    model_id: String,
    default_reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Deserialize)]
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
