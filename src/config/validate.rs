//! Config validation for `lait lint` (not `doctor` — the only callers are
//! `lint.rs` and `shell_tool.rs`; `doctor.rs` never calls into this module).
//! Each function here checks *every* entry in a map up front, returning one
//! message per violation, unlike the lazy per-use checks in `resolve.rs`
//! (`ModelDefinition::validate`, `check_shell_tool_definition` itself) that
//! only validate whichever single alias/layer/tool a particular run actually
//! resolves — see each function's own doc comment for why both exist.

use anyhow::{Result, bail};

use super::types::{ConfigFile, ShellToolDefinition, check_api_key_source};

/// `lait lint`'s view of [`check_api_key_source`]: checks the top-level
/// `api_key`/`api_key_cmd` pair and every `models:` entry's own
/// `provider.api_key`/`provider.api_key_cmd` — including fallback (2nd and
/// later) definitions in a `models:` alias, which `resolve_model_alias`
/// itself never validates since a run only resolves the first entry up
/// front (see `resolve_model_fallbacks`, which validates a fallback entry
/// lazily, only once that entry is actually attempted) — returning one
/// message per violation (empty when there are none) instead of bailing on
/// the first. Unlike `resolve_model_alias`/`resolve_endpoint`, which only
/// ever validate whichever single alias/layer a particular run actually
/// resolves, so a mistake in an alias/layer that CLI overrides currently
/// shadow, or in a fallback entry the primary endpoint never fails over to,
/// would otherwise go unnoticed until it actually gets used — potentially in
/// production rather than at `lait lint` time.
pub(crate) fn check_provider_api_key_sources(config: &ConfigFile) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = check_api_key_source(
        &config.api_key,
        &config.api_key_cmd,
        "top-level configuration",
    ) {
        errors.push(error.to_string());
    }
    let mut names: Vec<&String> = config.models.keys().collect();
    names.sort_unstable();
    for name in names {
        for (index, definition) in config.models[name].iter().enumerate() {
            let context = if index == 0 {
                format!("model definition {name:?}")
            } else {
                format!("model definition {name:?}'s fallback entry #{}", index + 1)
            };
            if let Err(error) = check_api_key_source(
                &definition.provider.api_key,
                &definition.provider.api_key_cmd,
                &context,
            ) {
                errors.push(error.to_string());
            }
        }
    }
    errors
}

/// Checks every `tools:` entry's `command`/`parameters` for the two things
/// `process::run_command` and the OpenAI tool-schema wire format both
/// require but `serde`'s own type-checking can't: a non-empty `command`
/// (`run_command` rejects an empty argv at runtime — see
/// `ShellToolDefinition::command`'s doc comment) and a `parameters` value
/// that is a JSON object (a non-object `parameters` would still deserialize
/// fine as `serde_json::Value`, but is not a valid JSON Schema object for
/// OpenAI's tool definition). Mirrors `check_provider_api_key_sources`: used
/// by both `lait lint` (every entry, whether referenced by a `tools:` list
/// anywhere or not) and `shell_tool::tools` (lazily, only for entries a
/// request actually names).
pub(crate) fn check_shell_tool_definition(
    name: &str,
    definition: &ShellToolDefinition,
) -> Result<()> {
    if definition.command.is_empty() {
        bail!("tool definition {name:?} has an empty 'command' list");
    }
    if !definition.parameters.is_object() {
        bail!("tool definition {name:?}'s 'parameters' must be a JSON object");
    }
    Ok(())
}

/// `lait lint`'s view of [`check_shell_tool_definition`]: checks every
/// `tools:` entry, returning one message per violation — see
/// `check_provider_api_key_sources`'s own doc comment for why this exists
/// separately from the lazy per-use check.
pub(crate) fn check_shell_tool_definitions(config: &ConfigFile) -> Vec<String> {
    let mut names: Vec<&String> = config.tools.keys().collect();
    names.sort_unstable();
    names
        .into_iter()
        .filter_map(|name| check_shell_tool_definition(name, &config.tools[name]).err())
        .map(|error| error.to_string())
        .collect()
}
