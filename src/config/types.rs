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

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::reasoning::ReasoningEffort;
use crate::response;

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
    /// The Jev-compatible decision API (TypeSafe's "System One" `POST
    /// /v1/systemone`) that workflow `decide:` steps and `lait decide` call.
    /// See `crate::jev` and [`JevConfig`].
    #[serde(default)]
    pub(crate) jev: JevConfig,
}

/// `jev:` — where Jev-compatible decision requests go. Every field is
/// optional: `base_url` defaults to TypeSafe's hosted API
/// (`crate::jev::DEFAULT_BASE_URL`), `model` to `jev-latest`, and no key
/// means no `Authorization` header (a local Jev-compatible server usually
/// needs none). `base_url`/`api_key` get `${VAR_NAME}` expansion, like the
/// top-level fields of the same name; `api_key_cmd` works as it does there.
/// Deliberately independent of the top-level `base_url`/`api_key`: those
/// name an OpenAI-compatible chat endpoint, which never serves
/// `/systemone`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JevConfig {
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) api_key_cmd: Option<CommandSpec>,
    pub(crate) model: Option<String>,
}

impl JevConfig {
    /// Project-wins merge with `api_key`/`api_key_cmd` as one unit — the
    /// same rule `load::merge_config` applies to the top-level pair.
    pub(crate) fn merge(global: Self, project: Self) -> Self {
        let (api_key, api_key_cmd) = if project.api_key.is_some() || project.api_key_cmd.is_some() {
            (project.api_key, project.api_key_cmd)
        } else {
            (global.api_key, global.api_key_cmd)
        };
        Self {
            base_url: project.base_url.or(global.base_url),
            api_key,
            api_key_cmd,
            model: project.model.or(global.model),
        }
    }
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
    /// An environment allowlist for the command's child process: when
    /// non-empty, the child sees *only* these variables (each value
    /// `${VAR_NAME}`-expandable, like `mcp_servers[].env` — see
    /// `expand_env_placeholders`), never the rest of `lait`'s own inherited
    /// environment (which may carry API keys or other secrets this command
    /// has no business seeing). Empty (the default) preserves the original
    /// behavior: the child inherits the whole parent environment unchanged.
    #[serde(default)]
    pub(crate) env: HashMap<String, String>,
    /// Pins the command's working directory (`${VAR_NAME}`-expandable, like
    /// `mcp_servers[].cwd`). `None` (the default) preserves the original
    /// behavior: the child inherits lait's own current directory.
    pub(crate) cwd: Option<String>,
}

fn default_tool_parameters() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

impl ShellToolDefinition {
    /// Expands `${VAR_NAME}` placeholders in `env`/`cwd` — the same
    /// expansion `McpServerConfig::resolve_transport` applies to
    /// `mcp_servers[].env`/`.cwd` — called lazily right before a call
    /// actually runs the command (see `shell_tool::call`), not at
    /// config-load time, matching every other `${VAR}`-expandable field in
    /// this crate.
    pub(crate) fn resolve_env_cwd(&self) -> Result<(HashMap<String, String>, Option<String>)> {
        let env = expand_map(&self.env)?;
        let cwd = self
            .cwd
            .as_deref()
            .map(expand_env_placeholders)
            .transpose()?;
        Ok((env, cwd))
    }
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
/// `default.compaction:` — periodically shrinks a tool loop's growing
/// message history by replacing older rounds with a model-generated summary,
/// so a long-running tool loop (many `mcp`/`subagents`/`tools` round trips)
/// can actually reach `max_tool_rounds` without first hitting the
/// *provider's* context-window/token limit (an error outside lait's own
/// control) — see `engine::transport`'s `compact_tool_loop`, the only
/// consumer. Deliberately not itself resolved through `overrides::
/// CapabilityOverrides` the way `mcp`/`max_tool_rounds`/`skills`/etc. are:
/// this is a `lait.config.yml`-global-only setting for now (no CLI flag, no
/// per-agent-file/per-workflow-node override) — a smaller, addable-later
/// surface, not a design ceiling.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompactionConfig {
    /// Compact every `trigger_rounds`-th round (`round % trigger_rounds ==
    /// 0`), right before that round's request is sent. At least one of this
    /// and `trigger_tokens` must be set, and whichever is set must be at
    /// least 1 — checked by `engine::transport`'s `maybe_compact` the first
    /// time a tool loop consults it.
    pub(crate) trigger_rounds: Option<usize>,
    /// Compact right before a round when the *previous* round's request
    /// reported at least this many `prompt_tokens` — a size-based trigger
    /// for a loop whose tool results vary wildly in length, where a fixed
    /// round count compacts too early or too late. Only fires when the
    /// server reports usage; a streamed round requests it for this purpose
    /// even without `--show-usage`.
    pub(crate) trigger_tokens: Option<u64>,
    /// How many of the tool loop's most recent messages survive a
    /// compaction verbatim (a leading system message, if any, always
    /// survives too, uncounted). Defaults to 4 — roughly the last two
    /// tool-call/tool-result round trips — when omitted.
    #[serde(default = "default_compaction_keep_last_n")]
    pub(crate) keep_last_n: usize,
    /// A `models:` alias (or raw model id) to send the summarization request
    /// to instead of the request's own model — typically a cheaper/faster
    /// one. Resolved against `lait.config.yml`'s own `models:` only (not a
    /// workflow's embedded `models:`), since this setting lives there too.
    pub(crate) model: Option<String>,
}

fn default_compaction_keep_last_n() -> usize {
    4
}

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
    /// See `CompactionConfig`'s own doc comment. `None` (the default) never
    /// compacts a tool loop's history at all — today's existing behavior.
    pub(crate) compaction: Option<CompactionConfig>,
    /// Whether `skills:` content is disclosed progressively: `false`/absent
    /// (the default) keeps today's behavior — every named skill's full body
    /// is always appended to the system prompt, no tool call involved. `true`
    /// appends only each skill's `name`/`description` (its frontmatter) and
    /// exposes a `skill__<name>` tool the model must call to read the rest —
    /// see `crate::skill::SkillCache::render_frontmatter` and
    /// `docs/usage/ja/skills.md`. config-file-global only, like `compaction`:
    /// no CLI flag, no per-agent-file/per-workflow-node override.
    pub(crate) skill_progressive_disclosure: Option<bool>,
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
            compaction: project.compaction.or(global.compaction),
            skill_progressive_disclosure: project
                .skill_progressive_disclosure
                .or(global.skill_progressive_disclosure),
        }
    }
}

/// A map of `mcp_servers:` name to its connection settings, as used by
/// `lait.config.yml`'s top-level `mcp_servers:`. `Arc`-wrapped (not a bare
/// `HashMap`) so `AppServices::new`/`doctor::check_mcp_servers` — both
/// called once per invocation, but each `.clone()`d off `ConfigFile` at
/// least once — only ever bump a refcount instead of deep-copying every
/// configured server's connection settings. `serde` deserializes into
/// `Arc<T>` transparently, so `#[serde(default)]` and direct YAML parsing
/// are unaffected; the only sites that needed to change are the two places
/// that mutate or merge these maps in place (`config::load`'s
/// `resolve_registry_paths_in_place`/`merge_config`), via `Arc::make_mut`
/// and a small `Arc`-aware counterpart to `merge_maps`.
pub(crate) type McpServerMap = Arc<HashMap<String, McpServerConfig>>;

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
    /// Whether this server may send an `elicitation/create` request
    /// mid-tool-call and have lait actually prompt on stdin/stderr for an
    /// answer. `false` (the default) always declines without prompting — an
    /// MCP server asking the user interactively for information is a trust
    /// escalation the same way an unrestricted `allowed_tools` is, so it's
    /// opt-in per server. See `mcp::elicitation` and
    /// `docs/usage/ja/mcp.md`'s elicitation section.
    #[serde(default)]
    pub(crate) allow_elicitation: bool,
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
pub(crate) type SkillMap = Arc<HashMap<String, PathBuf>>;

/// A map of `agents:` name to the path of its agent Markdown file, as used by
/// `lait.config.yml`'s top-level `agents:`. Each named entry can be made
/// available, via a `subagents:` list, as a tool the model itself may decide
/// to call mid-completion — unlike `agent:`/`workflow:` workflow nodes, which
/// wire in a fixed agent call at parse time. See `crate::subagent`.
pub(crate) type AgentMap = Arc<HashMap<String, PathBuf>>;

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

/// A `models:` definition's `pricing:` block: USD per 1,000,000 prompt/
/// completion tokens, for `--show-usage`/`lait compare` to turn a reported
/// [`response::Usage`] into an estimated cost. Entirely optional — every
/// caller treats "no `pricing:`" as "cost unknown" (`None`), never as zero,
/// the same None-vs-zero convention `response::Usage` itself uses for a
/// server that doesn't report usage at all.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pricing {
    pub(crate) input_per_1m: f64,
    pub(crate) output_per_1m: f64,
}

impl Pricing {
    /// Estimated USD cost of `usage` at these rates. Token counts are
    /// server-reported (untrusted, per `response::Usage::add`'s own
    /// comment), but a plain `as f64` conversion (rather than a
    /// saturating/checked one) is fine here: the result is a display
    /// estimate, not used for billing enforcement, and `u64::MAX` tokens
    /// converting to `f64` cannot panic or wrap the way integer overflow can.
    pub(crate) fn cost(&self, usage: response::Usage) -> f64 {
        (usage.prompt_tokens as f64 / 1_000_000.0) * self.input_per_1m
            + (usage.completion_tokens as f64 / 1_000_000.0) * self.output_per_1m
    }
}

/// Which OpenAI-compatible wire format a model's requests use. `ChatCompletions`
/// (the default, and the only option before this field existed) sends
/// `POST /chat/completions`; `Responses` sends `POST /responses` — OpenAI's
/// newer API, translated to and from the Chat Completions shapes the rest
/// of lait works in by `llm::responses` (tool calling, `--image`, and
/// `--stream` included). Provider-side `previous_response_id` chaining is
/// deliberately never used; see `docs/usage/ja/config.md`'s Responses API
/// section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiKind {
    #[default]
    ChatCompletions,
    Responses,
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
    /// Per-1M-token USD pricing, for `--show-usage`/`lait compare` to turn
    /// this model's reported token usage into an estimated cost — see
    /// `Pricing`'s own doc comment. Like `default_reasoning_effort`/etc.
    /// (see `FallbackCandidate`'s doc comment), this only ever applies from
    /// the *primary* `models:` definition: a fallback candidate's own
    /// `pricing:` (if it even set one) is never consulted, since cost is
    /// attributed to the logical request the same way the cache key and
    /// `gen_ai.request.model` trace attribute already are — see
    /// `engine::transport`'s `complete_recorded` doc comment.
    pricing: Option<Pricing>,
    /// See `ApiKind`'s own doc comment. Like `pricing`, only the *primary*
    /// `models:` definition's `api:` is ever consulted — a fallback
    /// candidate always uses Chat Completions regardless of the primary's
    /// setting (see `FallbackCandidate`, which deliberately carries no
    /// `api` field at all), so a Responses-API model's fallback silently
    /// downgrades to Chat Completions rather than failing outright. This
    /// matches `pricing`'s own "primary only" precedent.
    #[serde(default)]
    api: ApiKind,
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
            pricing: self.pricing,
            api: self.api,
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
    pub(crate) pricing: Option<Pricing>,
    /// See `ApiKind`. Always `ApiKind::ChatCompletions` for a bare model
    /// name (no `models:` alias) — see `resolve_model`'s own construction.
    pub(crate) api: ApiKind,
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
