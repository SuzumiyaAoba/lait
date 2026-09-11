//! `lait.config.yml`'s schema, discovery, and resolution — split into
//! [`types`] (the `serde`-deserialized schema), [`validate`] (`lait lint`'s
//! eager whole-file checks), [`resolve`] (model/endpoint resolution and
//! `${VAR}` expansion), and [`load`] (discovery/loading/merging of the
//! project and global files). Each stage only makes sense once the one
//! before it has run, so the split mostly relocates `pub(crate)`/`pub(super)`
//! boundaries without changing how any of it is used — see each submodule's
//! own doc comment for the handful of places two of them still call into
//! each other's private items (`pub(super)`, never wider).
//!
//! [`ConfigSource`] stays here rather than in [`load`]: it's the one type
//! every caller outside this module already names (`Cli` converts into it,
//! `lint::run`/`doctor` match on it directly), so it belongs at the front
//! door rather than behind a re-export.

use std::path::PathBuf;

use crate::cli::Cli;

mod load;
mod resolve;
mod types;
mod validate;

#[cfg(test)]
mod tests;

pub(crate) use load::{
    global_config_exists_cancellable, global_config_path, load_config, load_config_cancellable,
    resolve_config_path, resolve_config_path_cancellable,
};
pub(crate) use resolve::{
    FallbackCandidate, expand_env_placeholders, resolve_endpoint, resolve_fallback_endpoint,
    resolve_model, resolve_model_alias, resolve_model_fallbacks,
};
pub(crate) use types::{
    AgentMap, ApiKeySource, CommandSpec, ConfigFile, Endpoint, McpServerMap, McpTransport,
    ModelMap, PromptDefinition, ResolvedModel, ShellToolDefinition, SkillMap, ToolMap,
};
// Only ever constructed directly by test code elsewhere in the crate
// (`mcp/registry.rs`'s and `lint/tests.rs`'s own fixtures) — everywhere else
// reaches it through `ModelMap`/`McpServerMap`/`ConfigFile` without naming it.
#[cfg(test)]
pub(crate) use types::McpServerConfig;
pub(crate) use validate::{
    check_provider_api_key_sources, check_shell_tool_definition, check_shell_tool_definitions,
};

pub(crate) const CONFIG_FILE_NAME: &str = "lait.config.yml";

/// Where `load_config` should look for `lait.config.yml`, resolved once from
/// `Cli`'s two mutually exclusive flags (`--config`/`--no-config`, enforced
/// at the clap level via `conflicts_with`) rather than re-read from `Cli` at
/// every call site that used to take a bare `no_config: bool`.
#[derive(Clone, Debug)]
pub(crate) enum ConfigSource {
    /// Neither flag was given: search `CONFIG_FILE_NAME` starting at the
    /// current directory and walking up through its ancestors (like git
    /// looks for `.git`), merging the result (project layer winning) with
    /// the global config at [`global_config_path`] when that exists — see
    /// [`load_config`]. Falls back to [`ConfigFile::default`] if neither is
    /// found anywhere.
    Search,
    /// `--config PATH`: read exactly this file. Unlike `Search`, a missing
    /// file here is an error — the user named a specific path, so silently
    /// falling back to defaults would hide a typo. The global config is
    /// never consulted here — the user named a specific file, so silently
    /// blending in another one would be surprising.
    Explicit(PathBuf),
    /// `--no-config`: always [`ConfigFile::default`], no filesystem access
    /// (including the global config).
    Disabled,
}

impl From<&Cli> for ConfigSource {
    fn from(cli: &Cli) -> Self {
        if cli.no_config {
            Self::Disabled
        } else if let Some(path) = &cli.config {
            Self::Explicit(path.clone())
        } else {
            Self::Search
        }
    }
}
