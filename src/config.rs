use std::{collections::HashMap, fs, path::PathBuf};

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
    pub(crate) models: ModelMap,
    /// Named MCP servers, referenced by a `mcp:` list on the CLI/agent
    /// file/workflow node/`default:` block. See `crate::mcp::McpRegistry`.
    #[serde(default)]
    pub(crate) mcp_servers: McpServerMap,
    /// Named skill files, referenced by a `skills:` list on the agent
    /// file/workflow node/`default:` block. See `crate::skill`.
    #[serde(default)]
    pub(crate) skills: SkillMap,
    /// Named agent Markdown files, referenced by a `subagents:` list on the
    /// agent file/workflow node/`default:` block. See `crate::subagent`.
    #[serde(default)]
    pub(crate) agents: AgentMap,
}

/// The `default:` block shared by `lait.config.yml` and a workflow file: a
/// fallback model/reasoning effort used when a step (or, for the config file,
/// the CLI/env) doesn't specify its own.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DefaultSettings {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// A fallback system prompt for chat mode, used when neither `--system`
    /// nor `--system-file` is given. Agent files and workflow nodes bring
    /// their own system prompts and never read this.
    pub(crate) system: Option<String>,
    /// Fallback sampling `temperature`/`top_p`/`max_tokens`, each falling back
    /// independently (unlike `WorkflowDefaults::retry`, which falls back as a
    /// whole unit) when a step/CLI invocation doesn't set its own.
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
    /// Names of `mcp_servers:` entries whose tools are available by default,
    /// when a CLI invocation/agent file/workflow node doesn't set its own
    /// `mcp:`. Falls back independently, like `temperature`.
    pub(crate) mcp: Option<Vec<String>>,
    /// The maximum number of tool-call round trips a single completion
    /// request may take before lait gives up and errors, when `mcp:` names at
    /// least one server. Falls back independently, like `temperature`.
    pub(crate) max_tool_rounds: Option<usize>,
    /// Names of `skills:` entries whose content is appended to the system
    /// prompt by default, when an agent file/workflow node doesn't set its
    /// own `skills:`. Falls back independently, like `temperature`.
    pub(crate) skills: Option<Vec<String>>,
    /// Names of `agents:` entries made available as callable subagent tools
    /// by default, when an agent file/workflow node doesn't set its own
    /// `subagents:`. Falls back independently, like `temperature`.
    pub(crate) subagents: Option<Vec<String>>,
}

/// A map of `mcp_servers:` name to its connection settings, as used by
/// `lait.config.yml`'s top-level `mcp_servers:`.
pub(crate) type McpServerMap = HashMap<String, McpServerConfig>;

/// One `mcp_servers:` entry. Exactly one of `command` (stdio, a child
/// process) or `url` (streamable HTTP) must be set; see
/// `McpServerConfig::resolve_transport`, which is where that's enforced (not
/// here, matching how `ModelDefinition`'s `model_id` emptiness is checked
/// lazily in `resolve_model_alias` rather than at parse time).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServerConfig {
    /// The executable to spawn for a stdio server. Mutually exclusive with `url`.
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) env: HashMap<String, String>,
    pub(crate) cwd: Option<String>,
    /// The endpoint for a streamable-HTTP server. Mutually exclusive with `command`.
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) headers: HashMap<String, String>,
}

/// The transport settings for one MCP server, after resolving `${VAR}`
/// placeholders (see `expand_env_placeholders`) and deciding stdio vs. HTTP.
#[derive(Debug, Clone)]
pub(crate) enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
}

impl McpServerConfig {
    /// Resolves this entry into a transport, expanding `${VAR}` placeholders
    /// in every field the same way `base_url`/`api_key` are expanded (see
    /// `expand_env_placeholders`) — this entry is always config-sourced, never
    /// a CLI override. `name` is only used to name the server in error
    /// messages.
    pub(crate) fn resolve_transport(&self, name: &str) -> Result<McpTransport> {
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => bail!(
                "mcp_servers.{name} has both 'command' and 'url'; set exactly one (stdio vs. streamable HTTP)"
            ),
            (None, None) => bail!(
                "mcp_servers.{name} has neither 'command' nor 'url'; set exactly one (stdio vs. streamable HTTP)"
            ),
            (Some(command), None) => {
                let command = expand_env_placeholders(command)?;
                let args = self
                    .args
                    .iter()
                    .map(|arg| expand_env_placeholders(arg))
                    .collect::<Result<Vec<_>>>()?;
                let env = self
                    .env
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), expand_env_placeholders(value)?)))
                    .collect::<Result<HashMap<_, _>>>()?;
                let cwd = self
                    .cwd
                    .as_deref()
                    .map(expand_env_placeholders)
                    .transpose()?;
                Ok(McpTransport::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                })
            }
            (None, Some(url)) => {
                let url = expand_env_placeholders(url)?;
                let headers = self
                    .headers
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), expand_env_placeholders(value)?)))
                    .collect::<Result<HashMap<_, _>>>()?;
                Ok(McpTransport::Http { url, headers })
            }
        }
    }
}

/// A map of `skills:` name to the path of its skill file (or a directory
/// containing a `SKILL.md`), as used by `lait.config.yml`'s top-level
/// `skills:`. See `crate::skill::load_skill`.
pub(crate) type SkillMap = HashMap<String, PathBuf>;

/// A map of `agents:` name to the path of its agent Markdown file, as used by
/// `lait.config.yml`'s top-level `agents:`. Each named entry can be made
/// available, via a `subagents:` list, as a tool the model itself may decide
/// to call mid-completion — unlike `agent:`/`workflow:` workflow nodes, which
/// wire in a fixed agent call at parse time. See `crate::subagent`.
pub(crate) type AgentMap = HashMap<String, PathBuf>;

/// A map of model alias to its candidate definitions, as used by both
/// `lait.config.yml`'s top-level `models:` and a workflow file's `models:`.
pub(crate) type ModelMap = HashMap<String, Vec<ModelDefinition>>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelDefinition {
    pub(crate) provider: ProviderConfig,
    pub(crate) model_id: String,
    pub(crate) default_reasoning_effort: Option<ReasoningEffort>,
    pub(crate) default_temperature: Option<f64>,
    pub(crate) default_top_p: Option<f64>,
    pub(crate) default_max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderConfig {
    pub(crate) base_url: String,
    pub(crate) api_key: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ResolvedModel {
    pub(crate) model_id: String,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
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
        temperature: definition.default_temperature,
        top_p: definition.default_top_p,
        max_tokens: definition.default_max_tokens,
    }))
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
    use super::{ConfigFile, McpServerConfig, McpTransport, expand_with, resolve_model};
    use std::collections::HashMap;

    #[test]
    fn resolve_model_rejects_an_empty_model_name() {
        let config = ConfigFile::default();
        assert!(resolve_model(String::new(), &config).is_err());
        assert!(resolve_model("   ".to_owned(), &config).is_err());
    }

    #[test]
    fn resolve_model_passes_an_unaliased_name_through() {
        let config = ConfigFile::default();
        let resolved = resolve_model("some-model".to_owned(), &config).unwrap();
        assert_eq!(resolved.model_id, "some-model");
        assert!(resolved.base_url.is_none());
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

    fn stdio_config(command: &str) -> McpServerConfig {
        McpServerConfig {
            command: Some(command.to_owned()),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: HashMap::new(),
        }
    }

    fn http_config(url: &str) -> McpServerConfig {
        McpServerConfig {
            command: None,
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            url: Some(url.to_owned()),
            headers: HashMap::new(),
        }
    }

    #[test]
    fn resolves_a_stdio_server() {
        let transport = stdio_config("npx").resolve_transport("test").unwrap();
        match transport {
            McpTransport::Stdio { command, .. } => assert_eq!(command, "npx"),
            McpTransport::Http { .. } => panic!("expected a stdio transport"),
        }
    }

    #[test]
    fn resolves_an_http_server() {
        let transport = http_config("https://example.com/mcp")
            .resolve_transport("test")
            .unwrap();
        match transport {
            McpTransport::Http { url, .. } => assert_eq!(url, "https://example.com/mcp"),
            McpTransport::Stdio { .. } => panic!("expected an http transport"),
        }
    }

    #[test]
    fn rejects_a_server_with_neither_command_nor_url() {
        let config = McpServerConfig {
            command: None,
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: HashMap::new(),
        };
        let error = config.resolve_transport("test").unwrap_err();
        assert!(error.to_string().contains("neither"));
    }

    #[test]
    fn rejects_a_server_with_both_command_and_url() {
        let mut config = stdio_config("npx");
        config.url = Some("https://example.com/mcp".to_owned());
        let error = config.resolve_transport("test").unwrap_err();
        assert!(error.to_string().contains("both"));
    }

    #[test]
    fn expands_placeholders_in_stdio_env_and_args() {
        // SAFETY: single-threaded test-only env mutation, restored immediately.
        unsafe {
            std::env::set_var("LAIT_TEST_MCP_TOKEN", "secret");
        }
        let mut config = stdio_config("npx");
        config
            .env
            .insert("TOKEN".to_owned(), "${LAIT_TEST_MCP_TOKEN}".to_owned());
        let transport = config.resolve_transport("test").unwrap();
        unsafe {
            std::env::remove_var("LAIT_TEST_MCP_TOKEN");
        }
        match transport {
            McpTransport::Stdio { env, .. } => {
                assert_eq!(env.get("TOKEN").map(String::as_str), Some("secret"));
            }
            McpTransport::Http { .. } => panic!("expected a stdio transport"),
        }
    }
}
