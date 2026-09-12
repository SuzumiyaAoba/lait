//! `lait.config.yml`'s schema: every `serde`-deserialized type nested under
//! [`ConfigFile`], plus the handful of inherent methods that only make sense
//! pinned to their own type (`ToolPolicy::{allows, merge}`,
//! `DefaultSettings::merge`, `McpServerConfig::resolve_transport`,
//! `ModelDefinition::{validate, resolved_model, fallback_candidate}`).
//!
//! Named `types.rs` rather than `schema.rs` to avoid colliding with the
//! top-level `crate::schema` (JSON Schema validation of workflow node
//! inputs/outputs) — the same hazard `lint/report.rs`'s doc notes for the
//! unrelated top-level `crate::report`.

use std::{collections::HashMap, path::PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::reasoning::ReasoningEffort;

use super::resolve::{FallbackCandidate, expand_env_placeholders, expand_list, expand_map};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigFile {
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    /// Runs an external command once to obtain the top-level API key instead
    /// of embedding it in plaintext (or requiring a pre-exported environment
    /// variable, like `${VAR}` does) — e.g. a secrets-manager CLI (1Password,
    /// pass, gopass, aws secretsmanager, ...). Mutually exclusive with
    /// `api_key`; see `resolve_endpoint`, which enforces that and retains
    /// whichever layer's command actually wins for request-time resolution.
    pub(crate) api_key_cmd: Option<CommandSpec>,
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
    /// Named prompt templates, run via `-p`/`--prompt-name <NAME>` or
    /// `lait prompt <NAME>`. See `crate::prompt`.
    #[serde(default)]
    pub(crate) prompts: PromptMap,
    /// Named workflow files, runnable by name (`lait run <NAME>`, falling
    /// back to this map when `<NAME>` doesn't exist as a file) or listed via
    /// `lait workflow list`. Unlike `mcp_servers:`/`models:`, entries here
    /// get no `${VAR_NAME}` expansion (see `AGENTS.md`'s Security and
    /// Configuration section) — a path is not a place secrets belong. See
    /// `crate::workflow::resolve_run_target`.
    #[serde(default)]
    pub(crate) workflows: WorkflowMap,
    /// An allow/deny list gating every MCP/subagent/shell tool call by its
    /// qualified name (`server__tool`/`agent__name`/`tool__name`, the same
    /// form `mcp::qualify_tool_name` produces), checked in
    /// `engine::execute_tool_calls` before a call is dispatched — in
    /// addition to (not instead of) a `mcp_servers.<name>.allowed_tools`
    /// entry, which only ever restricts that one server's own raw tool
    /// names. See [`ToolPolicy`].
    #[serde(default)]
    pub(crate) tool_policy: ToolPolicy,
    /// Named shell-command tools, referenced by a `tools:` list on the
    /// CLI/agent file/workflow node/`default:` block — an alternative to
    /// `mcp_servers:` for exposing a single local command (`rg`, `jq`, `gh`,
    /// ...) as a callable tool without standing up a whole MCP server. See
    /// `crate::shell_tool`.
    #[serde(default)]
    pub(crate) tools: ToolMap,
}

/// A map of `tools:` name to its shell-command definition, as used by
/// `lait.config.yml`'s top-level `tools:`. See `crate::shell_tool`.
pub(crate) type ToolMap = HashMap<String, ShellToolDefinition>;

/// One `tools:` entry: a local command exposed to the model as a callable
/// tool, without an MCP server. See `crate::shell_tool::call`, which runs
/// it, and `crate::shell_tool::tools`, which turns it into an OpenAI tool
/// schema.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShellToolDefinition {
    /// Shown to the model as the tool's description. `None` is allowed at
    /// the config-parse level (nothing here requires it), but a model
    /// generally calls a tool more reliably when it has one.
    pub(crate) description: Option<String>,
    /// The command to exec — `command[0]` is the program, `command[1..]` its
    /// arguments. Each element is rendered as a handlebars template (see
    /// `crate::template::render`) against the model's JSON call arguments as
    /// `{{ input.<field> }}` before running, the same template engine and
    /// `input`/`field` access pattern a workflow's own `prompt:`/`command:`
    /// templates use. Run directly (`crate::process::run_command`), never
    /// through a shell — no element can inject a second command via `;`/`|`/
    /// backticks, even if it's built from an untrusted rendered value.
    /// Validated non-empty at first use (see `shell_tool::tools`) and by
    /// `lait lint`; `process::run_command` also returns a clear error for an
    /// empty argv instead of attempting to spawn it.
    pub(crate) command: Vec<String>,
    /// The JSON Schema describing the tool's call arguments, sent to the
    /// model verbatim as the OpenAI tool definition's `parameters`. Defaults
    /// to an empty-object schema (a tool that takes no arguments) when
    /// omitted.
    #[serde(default = "default_tool_parameters")]
    pub(crate) parameters: serde_json::Value,
    /// How many seconds this tool's command may run before it's killed and
    /// the call fails — see `shell_tool::DEFAULT_TOOL_TIMEOUT_SECS` for the
    /// default when this is unset.
    pub(crate) timeout: Option<u64>,
}

fn default_tool_parameters() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// `tool_policy:` (see [`ConfigFile::tool_policy`]): `deny` is checked
/// first — a match there rejects the call outright, regardless of `allow`.
/// Otherwise, an empty `allow` (the default) permits everything; a
/// non-empty `allow` permits only a qualified name matching one of its
/// patterns. Each pattern is matched with [`glob_match`] — a literal
/// name, or one with a single leading/trailing `*`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolPolicy {
    #[serde(default)]
    pub(crate) allow: Vec<String>,
    #[serde(default)]
    pub(crate) deny: Vec<String>,
}

impl ToolPolicy {
    /// Whether `qualified_name` (e.g. `mock__echo`, `agent__researcher`) may
    /// be called under this policy — see the type's own doc comment for the
    /// deny-then-allow precedence.
    pub(crate) fn allows(&self, qualified_name: &str) -> bool {
        if self
            .deny
            .iter()
            .any(|pattern| glob_match(pattern, qualified_name))
        {
            return false;
        }
        self.allow.is_empty()
            || self
                .allow
                .iter()
                .any(|pattern| glob_match(pattern, qualified_name))
    }

    /// Merges policy layers additively. A global deny is a safety floor that
    /// a project config cannot silently remove, while project allow rules can
    /// add capabilities permitted by the global layer.
    pub(super) fn merge(global: Self, project: Self) -> Self {
        let mut allow = global.allow;
        allow.extend(project.allow);
        let mut deny = global.deny;
        deny.extend(project.deny);
        Self { allow, deny }
    }
}

/// A minimal glob: `*substring*` (contains), `prefix*` (starts-with),
/// `*suffix` (ends-with), or a literal exact match — deliberately not a
/// general glob (no `?`, no wildcard elsewhere in the pattern, no crate
/// dependency for this). `*` alone matches everything. The both-ends case is
/// checked first: a naive "strip a trailing `*`, else strip a leading `*`"
/// order would take `*substring*` for a `prefix*` pattern with the literal
/// prefix `"*substring"`, which then can never match any real tool name
/// (qualified tool names never contain `*`) — silently turning an intended
/// "contains" deny/allow rule into a permanent no-op instead of an error.
fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(middle) = pattern.strip_prefix('*').and_then(|p| p.strip_suffix('*')) {
        return name.contains(middle);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    pattern == name
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
    /// Names of `tools:` entries made available as callable shell-command
    /// tools by default, when a CLI invocation/agent file/workflow node
    /// doesn't set its own `tools:`. Falls back independently, like `mcp`.
    /// See `crate::shell_tool`.
    pub(crate) tools: Option<Vec<String>>,
    /// Whether to render chat's response as Markdown for terminal display by
    /// default, when `--render` isn't passed. See `crate::render`.
    pub(crate) render: Option<bool>,
    /// Whether to record runs in `lait history` by default (`true` unless
    /// set to `false` here), when `--no-history` isn't passed. See
    /// `crate::history`.
    pub(crate) history: Option<bool>,
    /// Whether to cache completion responses on disk under `.lait/cache/` by
    /// default (`false` unless set to `true` here), when neither
    /// `--cache`/`--no-cache` is passed. See `crate::cache`.
    pub(crate) cache: Option<bool>,
    /// How many seconds a cached response stays valid, when set. A cache hit
    /// older than this is treated as a miss (the request is sent for real
    /// and the cache entry is refreshed). `None` (the default) means cached
    /// responses never expire on their own. See `crate::cache`.
    pub(crate) cache_ttl: Option<u64>,
}

impl DefaultSettings {
    /// Merges a lower-priority config layer with a project layer. Every
    /// setting is independent: a project value wins when present, otherwise
    /// the lower-priority value remains available as a fallback.
    pub(super) fn merge(global: Self, project: Self) -> Self {
        Self {
            model: project.model.or(global.model),
            reasoning_effort: project.reasoning_effort.or(global.reasoning_effort),
            system: project.system.or(global.system),
            temperature: project.temperature.or(global.temperature),
            top_p: project.top_p.or(global.top_p),
            max_tokens: project.max_tokens.or(global.max_tokens),
            mcp: project.mcp.or(global.mcp),
            max_tool_rounds: project.max_tool_rounds.or(global.max_tool_rounds),
            skills: project.skills.or(global.skills),
            subagents: project.subagents.or(global.subagents),
            tools: project.tools.or(global.tools),
            render: project.render.or(global.render),
            history: project.history.or(global.history),
            cache: project.cache.or(global.cache),
            cache_ttl: project.cache_ttl.or(global.cache_ttl),
        }
    }
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
    /// Restricts which of this server's tools the model may call. `None`
    /// (the field omitted) means unrestricted — every tool the server
    /// advertises is callable, matching lait's behavior before this field
    /// existed. `Some(vec![])` (an explicit empty list) means the opposite:
    /// no tool on this server may be called at all. These two are
    /// deliberately distinguishable (hence `Option<Vec<_>>` rather than a
    /// bare `Vec` defaulting to empty) — see `McpRegistry::call`, which
    /// enforces this before ever opening a connection to the server.
    pub(crate) allowed_tools: Option<Vec<String>>,
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
                let args = expand_list(&self.args)?;
                let env = expand_map(&self.env)?;
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
                let headers = expand_map(&self.headers)?;
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

/// A map of `prompts:` name to its template definition, as used by
/// `lait.config.yml`'s top-level `prompts:`. See `crate::prompt`.
pub(crate) type PromptMap = HashMap<String, PromptDefinition>;

/// A map of `workflows:` name to the path of its workflow YAML file, as used
/// by `lait.config.yml`'s top-level `workflows:`. Resolved relative to the
/// directory containing the `lait.config.yml` that defined it, not the
/// current working directory — see `crate::workflow::resolve_run_target`.
pub(crate) type WorkflowMap = HashMap<String, PathBuf>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptDefinition {
    /// The handlebars template rendered against `{{ input }}` (the CLI
    /// PROMPT/INPUT argument or piped stdin) and `{{ vars.<key> }}` (this
    /// entry's `vars:` defaults, overridable per call with `--var
    /// KEY=VALUE`) — see `crate::template::render`.
    pub(crate) template: String,
    /// The model this prompt runs on when `--model`/`LLM_MODEL` doesn't
    /// override it. Falls back to `default.model` when unset here too.
    pub(crate) model: Option<String>,
    /// Default values for `{{ vars.<key> }}` placeholders in `template`,
    /// overridable per call with `--var key=value`.
    #[serde(default)]
    pub(crate) vars: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelDefinition {
    pub(super) provider: ProviderConfig,
    model_id: String,
    default_reasoning_effort: Option<ReasoningEffort>,
    default_temperature: Option<f64>,
    default_top_p: Option<f64>,
    default_max_tokens: Option<u32>,
}

impl ModelDefinition {
    pub(super) fn validate(&self, context: &str) -> Result<()> {
        if self.model_id.trim().is_empty() {
            bail!("model_id in {context} must not be empty");
        }
        check_api_key_source(&self.provider.api_key, &self.provider.api_key_cmd, context)
    }

    pub(super) fn resolved_model(&self) -> ResolvedModel {
        ResolvedModel {
            model_id: self.model_id.clone(),
            base_url: Some(self.provider.base_url.clone()),
            api_key: self.provider.api_key.clone(),
            api_key_cmd: self.provider.api_key_cmd.clone(),
            reasoning_effort: self.default_reasoning_effort,
            temperature: self.default_temperature,
            top_p: self.default_top_p,
            max_tokens: self.default_max_tokens,
        }
    }

    pub(super) fn fallback_candidate(&self) -> FallbackCandidate {
        FallbackCandidate {
            model_id: self.model_id.clone(),
            base_url: self.provider.base_url.clone(),
            api_key: self.provider.api_key.clone(),
            api_key_cmd: self.provider.api_key_cmd.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderConfig {
    base_url: String,
    pub(super) api_key: Option<String>,
    /// See `ConfigFile::api_key_cmd`; mutually exclusive with `api_key` at
    /// this same provider level (a model definition may still fall back to
    /// the top-level `api_key`/`api_key_cmd` when it sets neither — see
    /// `resolve_endpoint`).
    pub(super) api_key_cmd: Option<CommandSpec>,
}

/// One `api_key_cmd:` value (top-level or `provider.api_key_cmd`): either a
/// shell-interpreted string — run via `sh -c` (`cmd /C` on Windows), so
/// pipes/quoting/subshells work the way a one-liner like `op read
/// op://Personal/OpenAI/api-key` expects — or a literal argv list, run
/// directly with no shell involved, for a command whose arguments should
/// never be shell-interpreted. See [`crate::secret::SecretResolver`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(untagged)]
pub(crate) enum CommandSpec {
    Shell(String),
    Argv(Vec<String>),
}

/// The selected API-key source for one endpoint. Selection and environment
/// expansion happen in `resolve_endpoint`; a [`Command`] is intentionally
/// retained as data so the command can be executed asynchronously only when
/// the request is about to be sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ApiKeySource {
    Absent,
    Literal(String),
    Command(CommandSpec),
}

/// A fully selected endpoint. It is pure configuration data: resolving a
/// command source never launches a process. `engine::RequestSettings` keeps
/// the `api_key` source until its first actual request and asks the shared
/// `secret::SecretResolver` to resolve it there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) base_url: String,
    pub(crate) api_key: ApiKeySource,
}

#[derive(Debug, Default)]
pub(crate) struct ResolvedModel {
    pub(crate) model_id: String,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) api_key_cmd: Option<CommandSpec>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
}

/// Rejects `api_key`/`api_key_cmd` set together at the same config layer —
/// checked eagerly wherever a layer's `ResolvedModel`/endpoint is built
/// (`resolve_model_alias` for a model definition's `provider.*`,
/// `resolve_endpoint` for the top-level `ConfigFile`), not just when that
/// layer actually ends up being used, so a misconfigured layer is never
/// hidden by a CLI/model-layer override taking precedence over it. `context`
/// names the layer for the error message (e.g. `"model definition
/// \"cloud\""`, `"top-level configuration"`).
pub(super) fn check_api_key_source(
    api_key: &Option<String>,
    api_key_cmd: &Option<CommandSpec>,
    context: &str,
) -> Result<()> {
    if api_key.is_some() && api_key_cmd.is_some() {
        bail!("{context} sets both 'api_key' and 'api_key_cmd'; set exactly one");
    }
    Ok(())
}
