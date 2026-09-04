//! The LLM request/response layer: resolving a completion request's
//! settings, sending it (including the MCP/subagent tool-call loop), and
//! calling an agent file's own template/schema/completion pipeline. Shared by
//! every caller that ultimately talks to a model — chat, `lait prompt`,
//! `lait agent run`, and a workflow node's `agent`/`prompt` action.

use std::{
    borrow::Cow,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::{
    error::OpenAIError,
    types::chat::{ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat},
};
use futures_util::StreamExt;

use crate::{
    agent::AgentFile,
    async_io, cache,
    cli::ReasoningEffort,
    config::{self, ConfigFile, ModelMap},
    llm, mcp, nesting, process, response, schema, shell_tool, skill, subagent, template, usage,
    workflow,
};

/// The maximum number of tool-call round trips a single completion request
/// may take (see `RequestSettings::complete`) before lait gives up and
/// errors instead of looping forever on a model that keeps calling tools.
/// Overridable per CLI invocation/agent file/workflow node/`default:` via
/// `max_tool_rounds`.
const DEFAULT_MAX_TOOL_ROUNDS: usize = 8;

/// The loaded config file, the MCP registry, the skill cache, the subagent
/// registry, and the run's top-level cancellation source for the whole
/// `lait`/`lait agent run`/`lait run` invocation — unlike `WorkflowScope`,
/// none of these change at a `workflow:` nesting boundary, so the same
/// `&AppContext` flows unchanged through every
/// `run_steps`/`execute_step_with_retry`/`execute_step` call (and, for
/// `call_agent`/`RequestSettings::complete`, through a subagent call's own
/// recursive completion too — see `call_subagent_tool`). Bundled into one
/// struct (rather than five parameters) purely to keep those functions'
/// argument counts under clippy's `too_many_arguments` threshold. Owns
/// everything (an `Arc<ConfigFile>`, not a borrow) rather than borrowing
/// `file_config` for a lifetime, so an `Arc<AppContext>` clone can move into
/// a `tokio::spawn`ed task for `parallel`/concurrent `for_each` (see
/// `workflow::exec::run_steps`) without that task's future needing to
/// outlive a borrow.
pub(crate) struct AppContext {
    pub(crate) file_config: Arc<ConfigFile>,
    pub(crate) registry: mcp::McpRegistry,
    pub(crate) skill_cache: skill::SkillCache,
    pub(crate) agent_registry: subagent::AgentRegistry,
    /// Every completion request's server-reported token usage, recorded by
    /// `RequestSettings::complete` and summarized when `--show-usage` asks
    /// for it.
    pub(crate) usage: usage::UsageTally,
    /// This invocation's own cancellation source, if any — the value every
    /// top-level `run_steps`/`complete` call seeds its own cancellation
    /// chain from (a node's own `timeout`/nested `workflow:` call then
    /// derives further child tokens off of that seed, see
    /// `execute_step_with_retry`). Set via `with_cancel` by every async
    /// command handler in `app.rs`/`repl.rs`, from the process-wide token
    /// `signal::spawn_handler` cancels on Ctrl-C — `None` only for a caller
    /// that never calls `with_cancel` (none currently; kept `Option` so a
    /// future non-interactive caller, e.g. a library embedding, can still
    /// opt out).
    pub(crate) cancel: Option<tokio_util::sync::CancellationToken>,
    /// `lait run --var KEY=VALUE` overrides (see `cli::VarArgs`), exposed to
    /// workflow templates as `{{ vars.<key> }}` and to jq filters as
    /// `$vars.<key>`. Empty for every caller but `app::run_workflow` — see
    /// `with_vars`.
    pub(crate) vars: serde_json::Map<String, serde_json::Value>,
    /// Whether `complete_recorded` should check/populate the response disk
    /// cache (`--cache`/`default.cache`, see `crate::cache`) for this
    /// invocation. `false` (the default) for a caller that never calls
    /// `with_cache` — resolved once per invocation, from the same CLI/config
    /// precedence for every caller (`app::run`), so a workflow's subagent
    /// calls and a nested `workflow:` call all inherit it unchanged, the
    /// same way `cancel` does.
    pub(crate) cache_enabled: bool,
    /// How many seconds a cache hit stays valid, when `cache_enabled`. `None`
    /// means cached responses never expire on their own. See `crate::cache`.
    pub(crate) cache_ttl: Option<u64>,
    /// Whether `execute_tool_calls` should interactively confirm each tool
    /// call on stdin/stderr before running it (`--approve-tools`), in
    /// addition to (never instead of) `file_config.tool_policy`'s allow/deny
    /// gate. `false` for a caller that never calls `with_approve_tools`.
    pub(crate) approve_tools: bool,
    /// Qualified tool names (see `mcp::qualify_tool_name`) the user has
    /// answered `a` for under `--approve-tools`, so `execute_tool_calls`
    /// stops asking about that name for the rest of this run. A `Mutex`
    /// (like `usage`'s own interior mutability) rather than requiring `&mut
    /// AppContext` — `execute_tool_calls` only ever holds a shared `&
    /// AppContext`, the same as every other tool-loop call.
    pub(crate) always_approved_tools: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl AppContext {
    /// Builds the registries/cache over `file_config`'s named entries. Cheap:
    /// each registry gets its own `Arc` clone of just the map it needs (MCP
    /// connections, skill files, and agent files are all loaded lazily on
    /// first use, so cloning the (typically small) name/path maps up front
    /// costs far less than any of that).
    pub(crate) fn new(file_config: Arc<ConfigFile>) -> Self {
        Self {
            registry: mcp::McpRegistry::new(Arc::new(file_config.mcp_servers.clone())),
            skill_cache: skill::SkillCache::new(Arc::new(file_config.skills.clone())),
            agent_registry: subagent::AgentRegistry::new(Arc::new(file_config.agents.clone())),
            file_config,
            usage: usage::UsageTally::default(),
            cancel: None,
            vars: serde_json::Map::new(),
            cache_enabled: false,
            cache_ttl: None,
            approve_tools: false,
            always_approved_tools: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Sets this context's `vars` (see the field doc), returning `self` for
    /// use in a builder chain at the call site (`app::run_workflow`).
    pub(crate) fn with_vars(mut self, vars: serde_json::Map<String, serde_json::Value>) -> Self {
        self.vars = vars;
        self
    }

    /// Sets this context's `cache_enabled`/`cache_ttl` (see the field docs) —
    /// every async command handler in `app.rs`/`repl.rs` calls this once,
    /// right where it builds the context, with the value `app::run` resolved
    /// from `--cache`/`--no-cache`/`default.cache`/`default.cache_ttl`.
    pub(crate) fn with_cache(mut self, enabled: bool, ttl: Option<u64>) -> Self {
        self.cache_enabled = enabled;
        self.cache_ttl = ttl;
        self
    }

    /// Sets this context's `approve_tools` (see the field doc) — every async
    /// command handler in `app.rs`/`repl.rs` calls this once, with the value
    /// `app::run` resolved from `--approve-tools`.
    pub(crate) fn with_approve_tools(mut self, approve_tools: bool) -> Self {
        self.approve_tools = approve_tools;
        self
    }

    /// Sets this context's `cancel` (see the field doc) — the process-wide
    /// token `signal::spawn_handler` cancels on Ctrl-C, so every
    /// `run_steps`/`complete`/blocking-I/O call downstream of this context
    /// observes it. Every async command handler in `app.rs` (and
    /// `repl::run`) calls this with the token `app::run` builds once per
    /// invocation.
    pub(crate) fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Drives `fut` to completion, then unconditionally shuts down the MCP
    /// registry before handing back `fut`'s result — on success or failure
    /// alike, so callers don't have to re-derive that ordering themselves.
    /// Every top-level `lait`/`lait agent run`/`lait run` invocation must
    /// call this once, at the end, instead of awaiting its work directly.
    pub(crate) async fn finish<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        let result = fut.await;
        self.registry.shutdown().await;
        result
    }
}

/// The reasoning-effort/temperature/top_p/max_tokens knobs a caller (CLI
/// invocation, agent file, or workflow step) may set for a single completion
/// request. Bundled into one struct (rather than four positional parameters)
/// because every layer of `resolve_request_settings`'s fallback chain treats
/// them identically: each field falls back independently to the next layer,
/// unlike e.g. `workflow::RetryDefinition`, which falls back as a whole unit.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SamplingOverrides {
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
}

impl SamplingOverrides {
    /// Folds `layers` field by field in priority order (the first layer with
    /// a field set wins, independently per field) — the shared
    /// implementation behind every caller's own precedence chain:
    /// `resolve_chat_settings`'s single `SharedChatArgs` layer,
    /// `resolve_step_settings`'s node > agent file > workflow default, and
    /// `agent_file_settings`'s single frontmatter layer. Does not include the
    /// model-alias/`file_config.default` tail every caller shares —
    /// `resolve_request_settings` adds those two layers after this.
    pub(crate) fn fold(layers: &[Self]) -> Self {
        Self {
            reasoning_effort: layers.iter().find_map(|layer| layer.reasoning_effort),
            temperature: layers.iter().find_map(|layer| layer.temperature),
            top_p: layers.iter().find_map(|layer| layer.top_p),
            max_tokens: layers.iter().find_map(|layer| layer.max_tokens),
        }
    }
}

/// The `mcp`/`max_tool_rounds`/`skills`/`subagents`/`tools` knobs a caller
/// may set for a single completion request, bundled the same way as
/// `SamplingOverrides` and for the same reason (keeps
/// `resolve_request_settings`'s argument count down; each field falls back
/// independently to `file_config.default`, not as a whole unit).
#[derive(Debug, Default, Clone)]
pub(crate) struct CapabilityOverrides {
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Option<Vec<String>>,
    pub(crate) subagents: Option<Vec<String>>,
    /// Names of `tools:` entries (see `config::ShellToolDefinition`) made
    /// available as callable shell-command tools during this request's tool
    /// loop. Falls back independently, like `mcp`.
    pub(crate) tools: Option<Vec<String>>,
}

impl CapabilityOverrides {
    /// Folds `layers` field by field in priority order — see
    /// `SamplingOverrides::fold`, which this mirrors.
    pub(crate) fn fold(layers: &[Self]) -> Self {
        Self {
            mcp: layers.iter().find_map(|layer| layer.mcp.clone()),
            max_tool_rounds: layers.iter().find_map(|layer| layer.max_tool_rounds),
            skills: layers.iter().find_map(|layer| layer.skills.clone()),
            subagents: layers.iter().find_map(|layer| layer.subagents.clone()),
            tools: layers.iter().find_map(|layer| layer.tools.clone()),
        }
    }
}

/// The new-turn inputs shared by `RequestSettings::complete`/
/// `complete_stream`: the system prompt, any prior turns from a resumed
/// `--session` (empty for every caller but chat), the new user-role prompt
/// text, and any `--image` attachments for it (empty for every caller but
/// chat). Bundled into one struct, like `SamplingOverrides`/
/// `CapabilityOverrides` above, to keep `complete`'s argument count under
/// clippy's `too_many_arguments` threshold.
pub(crate) struct PromptTurn<'a> {
    pub(crate) system_prompt: Option<&'a str>,
    pub(crate) history: &'a [ChatCompletionRequestMessage],
    pub(crate) prompt: &'a str,
    pub(crate) image_urls: &'a [String],
}

impl<'a> PromptTurn<'a> {
    /// A turn with no prior history and no image attachments — every caller
    /// but chat's own (`run_chat`/`repl::run_turn`, which have a real
    /// `--session`/`--image` history to carry).
    pub(crate) fn simple(system_prompt: Option<&'a str>, prompt: &'a str) -> Self {
        Self {
            system_prompt,
            history: &[],
            prompt,
            image_urls: &[],
        }
    }
}

/// The model/base-URL/API-key/sampling settings for a single completion
/// request, after resolving aliases and applying every fallback layer.
pub(crate) struct RequestSettings {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) resolved_model: config::ResolvedModel,
    /// Further `models:` alias definitions to fall back to, in order, when
    /// the primary endpoint above fails with a retryable error (a
    /// connection failure/timeout, or a 5xx/429/408 response) — see
    /// `complete_recorded`/`complete_stream`'s shared `attempt_with_fallback`
    /// and `docs/usage/ja/config.md`'s フォールバック section. Empty when
    /// `model_name` wasn't resolved from a `models:` alias, or a
    /// `--base-url`/`--api-key` override collapsed every candidate into one
    /// (see `resolve_request_settings`).
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

/// Combines a caller's own system prompt (an agent's rendered template, or
/// `None` for a plain `prompt:`/chat call) with a request's rendered skill
/// content, if any (see `skill::SkillCache::render`). The skill content is
/// appended after `base`, under a `---` delimiter, so the caller's own
/// instructions lead and a skill's own Markdown heading structure stays
/// visually distinct from them. Returns `base` unchanged (borrowed, no copy)
/// when there's no skill content to append — the common case, since most
/// requests don't set `skills:`.
fn with_skills<'a>(base: Option<&'a str>, skills_text: Option<&str>) -> Option<Cow<'a, str>> {
    match (base, skills_text) {
        (None, None) => None,
        (Some(base), None) => Some(Cow::Borrowed(base)),
        (None, Some(skills_text)) => Some(Cow::Owned(skills_text.to_owned())),
        (Some(base), Some(skills_text)) => {
            Some(Cow::Owned(format!("{base}\n\n---\n\n{skills_text}")))
        }
    }
}

/// One tool call's pre-dispatch decision — made for every call in a round
/// before any of them actually run, see `execute_tool_calls`'s own doc
/// comment on why this has to happen sequentially and up front rather than
/// inside the concurrent dispatch below.
enum ToolDecision {
    Allow,
    Deny(String),
}

/// Checks `qualified_name` against `env.file_config.tool_policy` (see
/// `config::ToolPolicy`) and, when `env.approve_tools` is set and the policy
/// didn't already deny it, interactively confirms the call — `y`/`n`/`a`,
/// via `prompt_tool_approval`. This is the *only* place either gate is
/// enforced; `McpRegistry::call`'s own `allowed_tools` check still applies
/// underneath it for an MCP tool (the two are independent, both must pass).
/// `command_preview` renders the argv a shell tool call would actually exec
/// (see `shell_tool::preview_argv`) for display alongside the model's raw
/// arguments — `None` for an MCP/subagent call, which have no such rendering
/// step. Taken as a closure rather than the rendered `Option<String>` itself
/// so the parse+render only happens on the path that actually reaches
/// `prompt_tool_approval` below — never for a call `tool_policy` denies
/// outright, approval isn't enabled for, or is already in
/// `always_approved_tools`.
async fn tool_decision(
    env: &AppContext,
    qualified_name: &str,
    arguments: &str,
    command_preview: impl FnOnce() -> Option<String>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<ToolDecision> {
    if !env.file_config.tool_policy.allows(qualified_name) {
        return Ok(ToolDecision::Deny(format!(
            "denied by 'tool_policy' in {}",
            config::CONFIG_FILE_NAME
        )));
    }
    if !env.approve_tools {
        return Ok(ToolDecision::Allow);
    }
    if env
        .always_approved_tools
        .lock()
        .expect("always_approved_tools lock poisoned")
        .contains(qualified_name)
    {
        return Ok(ToolDecision::Allow);
    }
    let command_preview = command_preview();
    match prompt_tool_approval(
        qualified_name,
        arguments,
        command_preview.as_deref(),
        cancellation,
    )
    .await?
    {
        ToolApprovalAnswer::Once => Ok(ToolDecision::Allow),
        ToolApprovalAnswer::Always => {
            env.always_approved_tools
                .lock()
                .expect("always_approved_tools lock poisoned")
                .insert(qualified_name.to_owned());
            Ok(ToolDecision::Allow)
        }
        ToolApprovalAnswer::Deny => Ok(ToolDecision::Deny(
            "denied interactively (--approve-tools)".to_owned(),
        )),
    }
}

enum ToolApprovalAnswer {
    Once,
    Always,
    Deny,
}

/// Prompts on stderr and reads one `y`/`n`/`a` answer from stdin for
/// `--approve-tools` — see `workflow::ask::run_ask`, whose TTY-detection and
/// non-interactive-is-an-error reasoning this mirrors exactly (a
/// non-interactive stdin has no one to answer and no way to tell a closed
/// pipe from a slow human, so this fails fast rather than hanging or
/// silently denying). Unlike `run_ask`, there is no `default:` to fall back
/// to here — `--approve-tools` without a terminal is simply a
/// misconfiguration to report, not a case with a sensible default answer.
async fn prompt_tool_approval(
    name: &str,
    arguments: &str,
    command_preview: Option<&str>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<ToolApprovalAnswer> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!(
            "'--approve-tools' requires an interactive stdin to confirm calling '{name}', but \
             stdin is not a terminal"
        );
    }
    eprintln!("tool call: {name}");
    eprintln!("arguments: {arguments}");
    // A shell tool's `command:` template can transform the arguments above
    // into something quite different from what they look like on their own
    // (e.g. splicing a path into a larger shell one-liner) — show the
    // actual argv about to run so approval is informed by what will really
    // execute, not just the model's raw JSON.
    if let Some(command_preview) = command_preview {
        eprintln!("command: {command_preview}");
    }
    eprint!("allow this call? [y(es)/n(o)/a(lways for this tool)] ");
    let name = name.to_owned();
    async_io::run_blocking(
        move |_cancelled| read_tool_approval_answer(&name),
        cancellation,
    )
    .await
}

/// The blocking half of `prompt_tool_approval`, run on a dedicated thread via
/// `async_io::run_blocking` the same way `workflow::ask::run_ask`'s own
/// blocking stdin read is. No re-prompt loop on a bad answer, for the same
/// reason `ask.rs`'s `validate_choice` doesn't retry: stdin may not be
/// interactive in every sense, and looping risks hanging rather than ever
/// finishing.
fn read_tool_approval_answer(name: &str) -> Result<ToolApprovalAnswer> {
    use std::io::{BufRead, Write};
    std::io::stderr().flush().ok();
    let mut buffer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buffer)
        .context("failed to read from stdin")?;
    let answer = process::strip_one_trailing_line_ending(buffer);
    match answer.as_str() {
        "y" | "Y" => Ok(ToolApprovalAnswer::Once),
        "a" | "A" => Ok(ToolApprovalAnswer::Always),
        "n" | "N" => Ok(ToolApprovalAnswer::Deny),
        other => bail!(
            "unrecognized answer {other:?} to 'allow this call?' for tool '{name}'; expected \
             'y', 'n', or 'a'"
        ),
    }
}

/// Checks that no qualified tool name is claimed by more than one of the
/// three tool sources a request can combine — `mcp::qualify_tool_name`
/// prefixes each source differently (`<server>__`/`agent__`/`tool__`), so a
/// collision only happens if two *different* servers/agents/shell tools
/// happen to render to the same sanitized name (or a `tools:` entry is
/// literally named the same as an `agents:` entry, etc.). Shared by
/// `complete`/`complete_stream`, which both assemble the same three sets.
fn check_tool_name_collisions(
    mcp_tool_set: &mcp::ToolSet,
    subagent_tool_set: &subagent::ToolSet,
    shell_tool_set: &shell_tool::ToolSet,
) -> Result<()> {
    for name in subagent_tool_set.names() {
        if mcp_tool_set.contains(name) {
            bail!("tool name collision: an MCP tool and a subagent both qualify to '{name}'");
        }
    }
    for name in shell_tool_set.names() {
        if mcp_tool_set.contains(name) {
            bail!("tool name collision: an MCP tool and a shell tool both qualify to '{name}'");
        }
        if subagent_tool_set.subagent_name(name).is_some() {
            bail!("tool name collision: a subagent and a shell tool both qualify to '{name}'");
        }
    }
    Ok(())
}

/// Appends one round's tool-call turn to `messages`: an `assistant` message
/// carrying `tool_calls` (plus whatever `content` preceded them, if any),
/// then the `tool`-role results of actually running each call — against
/// `mcp_tool_set`, `subagent_tool_set`, or `shell_tool_set`, whichever
/// qualifies the call's name (see `mcp::qualify_tool_name`). Shared by
/// `complete`'s non-streamed
/// tool loop and `complete_stream`'s streamed one (see
/// `response::StreamToolCallAccumulator`, which reassembles a streamed
/// round's fragments into the same `&[response::ToolCall]` shape a
/// non-streamed response already carries, so both loops converge on this one
/// dispatch).
///
/// Every call is checked against `tool_policy`/`--approve-tools` (see
/// `tool_decision`) *before* any of them run, one at a time in call order —
/// deliberately not folded into the concurrent dispatch below, because
/// `--approve-tools` prompts on stderr/stdin and concurrent prompts for
/// several calls in the same round would interleave into something
/// unreadable (and unanswerable). A denied call never reaches
/// `McpRegistry::call`/`call_subagent_tool` at all; it gets a `tool`-role
/// message carrying the denial reason instead, so the model sees it as a
/// failed call and the tool loop continues rather than the whole request
/// failing.
///
/// A model turn's *allowed* calls are independent by construction (it
/// couldn't have seen one call's result before deciding on another), so
/// they're run concurrently rather than one at a time; `try_join_all`
/// preserves `tool_calls`' order regardless of completion order, so the
/// appended `tool`-role messages stay in a stable, deterministic order.
#[allow(clippy::too_many_arguments)]
async fn execute_tool_calls(
    tool_calls: &[response::ToolCall],
    content: Option<&str>,
    messages: &mut Vec<ChatCompletionRequestMessage>,
    mcp_tool_set: &mcp::ToolSet,
    subagent_tool_set: &subagent::ToolSet,
    shell_tool_set: &shell_tool::ToolSet,
    env: &AppContext,
    active_agent_paths: &[PathBuf],
    round: usize,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<()> {
    messages.push(llm::assistant_tool_call_message(tool_calls, content)?);

    let mut decisions = Vec::with_capacity(tool_calls.len());
    for tool_call in tool_calls {
        // A shell tool call's `command:` template can render to something
        // quite different from its raw JSON arguments — see
        // `tool_decision`'s `command_preview` parameter — so look up the
        // definition and render a preview whenever this call qualifies to a
        // shell tool. `None` for MCP/subagent calls, and for a shell tool
        // whose arguments/template can't be rendered right now (the actual
        // failure, if any, surfaces from `shell_tool::call` itself once the
        // call is allowed to run). `tool_decision` only invokes this closure
        // on the path that actually reaches its interactive
        // `--approve-tools` prompt, so it's free on every other path.
        let command_preview = || {
            shell_tool_set
                .tool_name(&tool_call.function.name)
                .and_then(|name| env.file_config.tools.get(name))
                .and_then(|definition| {
                    shell_tool::preview_argv(definition, &tool_call.function.arguments)
                })
        };
        decisions.push(
            tool_decision(
                env,
                &tool_call.function.name,
                &tool_call.function.arguments,
                command_preview,
                cancellation.clone(),
            )
            .await?,
        );
    }

    let tool_messages = futures_util::future::try_join_all(tool_calls.iter().zip(decisions).map(
        |(tool_call, decision)| async {
            let name = &tool_call.function.name;
            if let ToolDecision::Deny(reason) = decision {
                tracing::debug!(tool = %name, round, reason = %reason, "tool call denied");
                return llm::tool_result_message(&tool_call.id, reason);
            }
            tracing::debug!(
                tool = %name,
                arguments = %tool_call.function.arguments,
                round,
                "calling tool",
            );
            let result = if mcp_tool_set.contains(name) {
                env.registry
                    .call(
                        mcp_tool_set,
                        name,
                        &tool_call.function.arguments,
                        cancellation.clone(),
                    )
                    .await?
            } else if let Some(subagent_name) = subagent_tool_set.subagent_name(name) {
                call_subagent_tool(
                    subagent_name,
                    &tool_call.function.arguments,
                    env,
                    active_agent_paths,
                    cancellation.clone(),
                )
                .await?
            } else if let Some(tool_name) = shell_tool_set.tool_name(name) {
                let definition = &env.file_config.tools[tool_name];
                shell_tool::call(
                    definition,
                    &tool_call.function.arguments,
                    cancellation.clone(),
                )
                .await?
            } else {
                bail!("model called unknown tool '{name}'");
            };
            llm::tool_result_message(&tool_call.id, result)
        },
    ))
    .await?;
    messages.extend(tool_messages);
    Ok(())
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
fn is_fallback_eligible(error: &anyhow::Error) -> bool {
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
struct EndpointAttempt {
    base_url: String,
    api_key: String,
    model_id: String,
}

impl EndpointAttempt {
    fn primary(settings: &RequestSettings) -> Self {
        Self {
            base_url: settings.base_url.clone(),
            api_key: settings.api_key.clone(),
            model_id: settings.resolved_model.model_id.clone(),
        }
    }
}

impl RequestSettings {
    /// Sets `usage_label` — see that field's doc comment.
    pub(crate) fn with_usage_label(mut self, label: impl Into<String>) -> Self {
        self.usage_label = label.into();
        self
    }

    /// Builds an `llm::CompletionRequest` from these settings' sampling
    /// parameters (the same for every request `self` ever builds, and never
    /// affected by which endpoint candidate is being attempted — see
    /// `FallbackCandidate`'s doc comment) plus `endpoint` (the candidate
    /// currently being attempted) and the per-call `response_format`/
    /// `messages`/`tools`. Both `complete`'s tool loop and `complete_stream`
    /// go through here instead of repeating this field list at each call site.
    fn request<'a>(
        &'a self,
        endpoint: &'a EndpointAttempt,
        response_format: Option<ResponseFormat>,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: &'a [ChatCompletionTools],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> llm::CompletionRequest<'a> {
        llm::CompletionRequest {
            base_url: &endpoint.base_url,
            api_key: &endpoint.api_key,
            model_id: &endpoint.model_id,
            reasoning_effort: self.sampling.reasoning_effort,
            temperature: self.sampling.temperature,
            top_p: self.sampling.top_p,
            max_tokens: self.sampling.max_tokens,
            response_format,
            messages,
            tools,
            stream_include_usage: false,
            cancellation,
        }
    }

    /// Advances `(base_url, api_key, model_id)` past a failed attempt to the
    /// next `self.fallback_candidates` entry, resolving that candidate's own
    /// endpoint (and running its `api_key_cmd`, if it has one) right now —
    /// never earlier, so a candidate that's never attempted never runs its
    /// command. Returns `Ok(false)` (leaving the three unchanged) once
    /// `candidates` is exhausted, telling the caller to give up and return
    /// its original error instead. Shared by `complete_recorded`/
    /// `complete_stream`'s otherwise-identical fallback loops — see
    /// `is_fallback_eligible` for what actually triggers a call to this.
    fn advance_to_next_candidate(
        &self,
        env: &AppContext,
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
        let (next_base_url, next_api_key) =
            config::resolve_fallback_endpoint(candidate, &env.file_config)?;
        endpoint.base_url = next_base_url;
        endpoint.api_key = next_api_key;
        endpoint.model_id = candidate.model_id.clone();
        Ok(true)
    }

    /// Sends a completion request built from these settings, driving a
    /// tool-call loop when `self.mcp`/`self.subagents` names at least one MCP
    /// server or subagent: each round sends the growing message history to
    /// the model, and if it comes back with `tool_calls`, `env.registry`
    /// (for an MCP tool) or `call_subagent_tool` (for a subagent tool)
    /// executes them and their results are appended as `tool`-role messages
    /// before the next round. Ends either when a round produces no
    /// `tool_calls` (the model's final answer) or after
    /// `self.max_tool_rounds` rounds, whichever comes first.
    ///
    /// `response_format` is withheld from every round while tools are still
    /// in play and only attached to the final, tool-free round: many
    /// OpenAI-compatible servers, given a strict `json_schema` response
    /// format, force schema-conforming output and never emit `tool_calls` at
    /// all, which would silently stop tools from ever firing. See
    /// `docs/usage/ja/mcp.md`.
    ///
    /// `self.skills` (resolved against `env.skill_cache`, `lait.config.yml`'s
    /// top-level `skills:`) is appended to `system_prompt` before either path
    /// below ever sees it — see `with_skills`. `active_agent_paths` is every
    /// subagent file currently executing on this call stack (canonicalized);
    /// pass `&[]` for a top-level call (chat/`lait agent run`/a workflow
    /// step) and `call_subagent_tool` extends it for a subagent's own
    /// completion, so a subagent chain that cycles back to itself is caught
    /// the same way `workflow:` nesting is (see `MAX_SUBAGENT_DEPTH`).
    /// `turn.history`/`turn.image_urls` are only ever non-empty for chat's
    /// own call site (a resumed `--session`, `--image`); every other caller
    /// passes `PromptTurn { history: &[], image_urls: &[], .. }`, which
    /// reproduces the exact message shape this method built before either
    /// feature existed — see `llm::initial_messages`.
    pub(crate) async fn complete(
        &self,
        env: &AppContext,
        active_agent_paths: &[PathBuf],
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<response::ChatCompletionResponse> {
        let system_prompt = self
            .system_prompt_with_skills(&env.skill_cache, turn.system_prompt, cancellation.clone())
            .await?;
        let system_prompt = system_prompt.as_deref();

        if self.mcp.is_empty() && self.subagents.is_empty() && self.tools.is_empty() {
            let messages =
                llm::initial_messages(system_prompt, turn.history, turn.prompt, turn.image_urls)?;
            return self
                .complete_recorded(env, response_format, messages, &[], cancellation)
                .await;
        }

        let (mcp_tool_set, subagent_tool_set, shell_tool_set, tools) =
            self.assemble_tool_sets(env, cancellation.clone()).await?;

        let mut messages =
            llm::initial_messages(system_prompt, turn.history, turn.prompt, turn.image_urls)?;

        let mut round = 0usize;
        loop {
            round += 1;
            if round > self.max_tool_rounds {
                bail!(
                    "tool loop exceeded max_tool_rounds ({}) without the model producing a final response",
                    self.max_tool_rounds
                );
            }

            let response = self
                .complete_recorded(env, None, messages.clone(), &tools, cancellation.clone())
                .await?;

            let tool_calls = response::first_message(&response)
                .and_then(|message| message.tool_calls.as_ref())
                .filter(|tool_calls| !tool_calls.is_empty());

            let Some(tool_calls) = tool_calls else {
                if response_format.is_none() {
                    return Ok(response);
                }
                // The model stopped calling tools; re-issue the same history
                // once more with `response_format` attached, now that doing
                // so can no longer suppress a tool call.
                return self
                    .complete_recorded(env, response_format, messages, &[], cancellation.clone())
                    .await;
            };

            let content = response::first_message(&response).and_then(|message| message.content());
            execute_tool_calls(
                tool_calls,
                content,
                &mut messages,
                &mcp_tool_set,
                &subagent_tool_set,
                &shell_tool_set,
                env,
                active_agent_paths,
                round,
                cancellation.clone(),
            )
            .await?;
        }
    }

    /// Builds the three tool sets (`mcp:`, `subagents:`, `tools:`) `complete`/
    /// `complete_stream` dispatch calls against, plus their merged OpenAI-
    /// shaped `tools:` payload — shared by both since streamed and
    /// non-streamed requests assemble tools identically, only how each round
    /// is issued differs. `agent_registry.tools`/`shell_tool::tools` are both
    /// synchronous (they only read local subagent files/`file_config.tools`),
    /// so joining the MCP round trip with the subagent one lets both proceed
    /// together instead of paying the MCP latency before ever touching disk;
    /// `shell_tool::tools` is cheap enough to just call inline after.
    ///
    /// Callers must not call this when `self.mcp`/`self.subagents`/
    /// `self.tools` are all empty — that's the plain-completion fast path,
    /// handled separately above.
    async fn assemble_tool_sets(
        &self,
        env: &AppContext,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<(
        mcp::ToolSet,
        subagent::ToolSet,
        shell_tool::ToolSet,
        Vec<ChatCompletionTools>,
    )> {
        let (mut mcp_tool_set, mut subagent_tool_set) = tokio::try_join!(
            env.registry.tools(&self.mcp, cancellation.clone()),
            env.agent_registry
                .tools_cancellable(&self.subagents, cancellation.clone()),
        )?;
        let mut shell_tool_set = shell_tool::tools(&self.tools, &env.file_config.tools)?;
        check_tool_name_collisions(&mcp_tool_set, &subagent_tool_set, &shell_tool_set)?;
        // Only `.contains()`/`.subagent_name()`/`.tool_name()` (which read
        // `.index`, not `.tools`) are used by callers below, so `.tools`
        // doesn't need to survive past this merge — moving it out avoids
        // cloning every tool definition (including its full JSON `parameters`).
        let mut tools = std::mem::take(&mut mcp_tool_set.tools);
        tools.extend(std::mem::take(&mut subagent_tool_set.tools));
        tools.extend(std::mem::take(&mut shell_tool_set.tools));
        Ok((mcp_tool_set, subagent_tool_set, shell_tool_set, tools))
    }

    /// The one way `complete` sends a request: checks the response disk
    /// cache first when `env.cache_enabled` (see `crate::cache` and
    /// `docs/usage/ja/config.md`'s キャッシュ section — a hit skips the
    /// network entirely and is *not* recorded in `--show-usage`, since no
    /// request was actually sent), otherwise builds the request via
    /// `request`, awaits it, records the response's usage under
    /// `self.usage_label` — so no future call site can forget the recording
    /// and skew `--show-usage` — and, still only on a cache-enabled miss,
    /// writes the response back to the cache. Tries `self.fallback_candidates`
    /// in order after a retryable failure (see `is_fallback_eligible`/
    /// `advance_to_next_candidate`) before giving up. The cache key is
    /// always computed from the *primary* endpoint (`self.base_url`/
    /// `self.resolved_model.model_id`), never whichever fallback candidate
    /// actually served the request — the cache represents "what would this
    /// logical request return", not which of possibly several endpoints
    /// happened to answer it.
    async fn complete_recorded(
        &self,
        env: &AppContext,
        response_format: Option<ResponseFormat>,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: &[ChatCompletionTools],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<response::ChatCompletionResponse> {
        let cache_key = if env.cache_enabled {
            Some(cache::key(
                &self.base_url,
                &self.resolved_model.model_id,
                self.sampling,
                &messages,
                tools,
                response_format.as_ref(),
            )?)
        } else {
            None
        };
        if let Some(cache_key) = &cache_key {
            match cache::load(cache_key, env.cache_ttl) {
                Ok(Some(response)) => {
                    eprintln!("note: cache hit for {}", self.usage_label);
                    tracing::debug!(cache_key = %cache_key, "response cache hit");
                    return Ok(response);
                }
                Ok(None) => tracing::debug!(cache_key = %cache_key, "response cache miss"),
                Err(error) => tracing::debug!(
                    cache_key = %cache_key,
                    error = %error,
                    "failed to read response cache entry; treating it as a miss",
                ),
            }
        }

        let mut endpoint = EndpointAttempt::primary(self);
        let mut candidates = self.fallback_candidates.iter();
        loop {
            let request = self.request(
                &endpoint,
                response_format.clone(),
                messages.clone(),
                tools,
                cancellation.clone(),
            );
            match llm::complete(request).await {
                Ok(response) => {
                    env.usage.record_response(&self.usage_label, &response);
                    if let Some(cache_key) = &cache_key
                        && let Err(error) = cache::save(cache_key, &response)
                    {
                        tracing::debug!(error = %error, "failed to write response cache entry");
                    }
                    return Ok(response);
                }
                Err(error) if is_fallback_eligible(&error) => {
                    if !self.advance_to_next_candidate(
                        env,
                        &mut candidates,
                        &mut endpoint,
                        &error,
                    )? {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Sends one streamed request and returns the raw stream — the streaming
    /// counterpart of `complete_recorded`'s single-request-plus-fallback
    /// loop, minus the response disk cache (a stream is never cached; see
    /// `docs/usage/ja/config.md`'s キャッシュ section). `tools` lets a
    /// streamed round advertise MCP/subagent tools the same way a
    /// non-streamed round's `tools` slice does — `complete_stream` (below)
    /// is the only caller, and passes `&[]` for a request with no tool
    /// sources. Falls back through `self.fallback_candidates`, like
    /// `complete_recorded`, but only up to the point a candidate's
    /// `llm::complete_stream` call itself returns `Err` — once a stream is
    /// established (`Ok`), lait commits to it: a streamed `tool_calls`/
    /// content delta arriving from one candidate can't be silently resent to
    /// another mid-stream.
    async fn stream_endpoint(
        &self,
        env: &AppContext,
        response_format: Option<ResponseFormat>,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: &[ChatCompletionTools],
        include_usage: bool,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<llm::CompletionStream> {
        let mut endpoint = EndpointAttempt::primary(self);
        let mut candidates = self.fallback_candidates.iter();
        loop {
            let mut request = self.request(
                &endpoint,
                response_format.clone(),
                messages.clone(),
                tools,
                cancellation.clone(),
            );
            request.stream_include_usage = include_usage;
            match llm::complete_stream(request).await {
                Ok(stream) => return Ok(stream),
                Err(error) if is_fallback_eligible(&error) => {
                    if !self.advance_to_next_candidate(
                        env,
                        &mut candidates,
                        &mut endpoint,
                        &error,
                    )? {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Like [`RequestSettings::complete`], but streams each round's response
    /// to `output_path` (`None` for stdout) as it arrives instead of waiting
    /// for the full completion — driving the same MCP/subagent tool loop
    /// `complete` does when `self.mcp`/`self.subagents` names at least one
    /// tool source, reassembling each round's streamed `tool_calls`
    /// fragments (see `response::StreamToolCallAccumulator`) before handing
    /// them to the same `execute_tool_calls` dispatch `complete`'s
    /// non-streamed loop uses. `self.skills` is appended to `system_prompt`
    /// the same way as in `complete`. `include_usage` asks the server for a
    /// final usage chunk on every round (see
    /// `llm::CompletionRequest::stream_include_usage`); set it only when the
    /// caller will actually display it (`--show-usage`). `turn.history`/
    /// `turn.image_urls`/`active_agent_paths` behave exactly as in
    /// `complete` — see its doc comment. Returns the *last* round's
    /// [`StreamOutcome`] (the one whose content was actually the final
    /// answer) — an intermediate round's content, if any, was still streamed
    /// to `output_path` as it arrived, exactly like the final round's, since
    /// there is no way to know a round is not the last one until after it
    /// has already finished streaming.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_stream(
        &self,
        env: &AppContext,
        active_agent_paths: &[PathBuf],
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        include_usage: bool,
        show_reasoning: bool,
        output_path: Option<&Path>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<StreamOutcome> {
        let system_prompt = self
            .system_prompt_with_skills(&env.skill_cache, turn.system_prompt, cancellation.clone())
            .await?;
        let system_prompt = system_prompt.as_deref();

        if self.mcp.is_empty() && self.subagents.is_empty() && self.tools.is_empty() {
            let messages =
                llm::initial_messages(system_prompt, turn.history, turn.prompt, turn.image_urls)?;
            let stream = self
                .stream_endpoint(
                    env,
                    response_format,
                    messages,
                    &[],
                    include_usage,
                    cancellation.clone(),
                )
                .await?;
            return stream_response(stream, show_reasoning, output_path, false, cancellation).await;
        }

        let (mcp_tool_set, subagent_tool_set, shell_tool_set, tools) =
            self.assemble_tool_sets(env, cancellation.clone()).await?;

        let mut messages =
            llm::initial_messages(system_prompt, turn.history, turn.prompt, turn.image_urls)?;

        let mut round = 0usize;
        loop {
            round += 1;
            if round > self.max_tool_rounds {
                bail!(
                    "tool loop exceeded max_tool_rounds ({}) without the model producing a final response",
                    self.max_tool_rounds
                );
            }

            let stream = self
                .stream_endpoint(
                    env,
                    None,
                    messages.clone(),
                    &tools,
                    include_usage,
                    cancellation.clone(),
                )
                .await?;
            // Every round after the first must append rather than truncate
            // `output_path` — otherwise a later round's `File::create` would
            // wipe out whatever an earlier round already streamed to it.
            let outcome = stream_response(
                stream,
                show_reasoning,
                output_path,
                round > 1,
                cancellation.clone(),
            )
            .await?;

            if outcome.tool_calls.is_empty() {
                if response_format.is_none() {
                    return Ok(outcome);
                }
                // The model stopped calling tools; re-issue the same history
                // once more with `response_format` attached, now that doing
                // so can no longer suppress a tool call — mirrors
                // `complete`'s own re-issue. This round's own outcome is
                // discarded in favor of the reissue's, so — unlike the
                // `return Ok(outcome)` above, whose usage the caller records
                // from the returned `StreamOutcome` — its usage has to be
                // recorded here or it's lost entirely; see the fallthrough
                // branch below for why every non-final round needs this.
                if let Some(usage) = outcome.usage {
                    env.usage.record(&self.usage_label, usage);
                }
                let stream = self
                    .stream_endpoint(
                        env,
                        response_format,
                        messages,
                        &[],
                        include_usage,
                        cancellation.clone(),
                    )
                    .await?;
                return stream_response(stream, show_reasoning, output_path, true, cancellation)
                    .await;
            }

            // Unlike `complete_recorded` (the non-streamed tool loop's
            // single choke point, which records every round's usage as it
            // happens), this round's `StreamOutcome` is consumed by
            // `execute_tool_calls` below and never reaches a caller — only
            // the loop's *final* round is ever returned, and that's the one
            // `app::run_chat`/`repl::run_turn` record from the returned
            // `StreamOutcome`. Recording here is this round's only chance to
            // be counted at all; skipping it (as this loop did before) would
            // silently undercount `--show-usage` by every tool-calling round
            // but the last.
            if let Some(usage) = outcome.usage {
                env.usage.record(&self.usage_label, usage);
            }

            let content = if outcome.content.is_empty() {
                None
            } else {
                Some(outcome.content.as_str())
            };
            execute_tool_calls(
                &outcome.tool_calls,
                content,
                &mut messages,
                &mcp_tool_set,
                &subagent_tool_set,
                &shell_tool_set,
                env,
                active_agent_paths,
                round,
                cancellation.clone(),
            )
            .await?;
        }
    }

    /// Shared by `complete`/`complete_stream`: resolves `self.skills` against
    /// `skill_cache` and appends the result to `system_prompt` — see
    /// `with_skills`.
    async fn system_prompt_with_skills<'a>(
        &self,
        skill_cache: &skill::SkillCache,
        system_prompt: Option<&'a str>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Option<Cow<'a, str>>> {
        let skills_text = skill_cache.render(&self.skills, cancellation).await?;
        Ok(with_skills(system_prompt, skills_text.as_deref()))
    }
}

/// Consumes `stream`, writing each chunk's content delta to stdout as it
/// arrives (flushed immediately, since stdout is line-buffered and a delta
/// rarely ends in a newline). When `show_reasoning` is set, reasoning deltas
/// are written first, formatted like `response::format_response` formats a
/// complete response: a `Reasoning:` header before the first reasoning
/// delta, then a blank line before the first content delta. Reasoning deltas
/// are dropped when `show_reasoning` is unset, same as the non-streaming
/// path. Also accumulates any streamed `tool_calls` fragments (see
/// `response::StreamToolCallAccumulator`) — `RequestSettings::complete_stream`
/// is the only caller that ever sees a non-empty result, since it's the only
/// one that ever advertises tools on the request this stream answers.
/// Fails, like `response::response_content`, if the round ends with neither
/// content nor a tool call — a response with nothing at all to act on.
/// Returns the accumulated content text (for `--session`/`lait history` to
/// record — see `StreamOutcome`) alongside the usage carried by the final
/// chunk, when the request asked for one (see
/// `RequestSettings::complete_stream`'s `include_usage`) and the server
/// obliged. `output_path` redirects the content to a file (`-o`): the file
/// then holds the body alone, so reasoning deltas — normally written ahead of
/// the content on stdout — go to stderr instead. `append` opens that file in
/// append mode instead of truncating it — set by `complete_stream` for every
/// round after a tool loop's first, so an earlier round's already-streamed
/// content survives a later round's own file open; ignored when
/// `output_path` is `None`.
///
/// `cancellation` is raced against each chunk (via `async_io::
/// await_cancellation`, the same primitive `llm::complete`'s own
/// cancellation wraps), so a stream stuck waiting on the next chunk can be
/// abandoned instead of hanging until the server closes the connection —
/// previously nothing watched cancellation once the stream was established
/// (see `AppContext::cancel`'s doc comment: today's only live source is a
/// workflow step's own `timeout`, since nothing yet wires up a process-wide
/// source like Ctrl-C). On cancellation, a `-o` file is explicitly flushed
/// before returning the error, so it holds whatever content arrived before
/// the cancellation rather than being left empty by `BufWriter`'s buffering
/// (deltas to a file are batched, not flushed per-delta, unlike stdout's —
/// see below). A stream ending in some other error (a malformed chunk, a
/// dropped connection) does not get this treatment and can still leave an
/// empty `-o` file — a pre-existing gap this change doesn't address.
async fn stream_response(
    mut stream: llm::CompletionStream,
    show_reasoning: bool,
    output_path: Option<&Path>,
    append: bool,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<StreamOutcome> {
    use tokio::io::{AsyncWrite, AsyncWriteExt};

    // Opened once for the stream's whole lifetime: every delta write below
    // reuses the same handle, and nothing else prints to the content sink
    // while a response is streaming. `tokio::io::Stdout`/`tokio::fs::File`
    // are used (rather than `std::io::stdout().lock()`/`std::fs::File`) so
    // this async fn's writes never block the Tokio worker thread it runs
    // on — each is dispatched to Tokio's own blocking-I/O thread pool
    // internally.
    let mut stdout_writer;
    let mut file_writer;
    // Whether reasoning shares the content sink (the stdout presentation:
    // a `Reasoning:` header, then a blank line before the content).
    let reasoning_inline = output_path.is_none();
    let content_sink: &mut (dyn AsyncWrite + Unpin) = match output_path {
        None => {
            stdout_writer = tokio::io::stdout();
            &mut stdout_writer
        }
        Some(path) => {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(path)
                .await
                .with_context(|| format!("failed to create output file '{}'", path.display()))?;
            file_writer = tokio::io::BufWriter::new(file);
            &mut file_writer
        }
    };
    let mut wrote_reasoning = false;
    let mut wrote_content = false;
    let mut last_usage = None;
    let mut content_text = String::new();
    let mut tool_calls = response::StreamToolCallAccumulator::default();

    loop {
        let chunk = match async_io::await_cancellation(stream.next(), cancellation.clone()).await {
            async_io::CancellationResult::Cancelled => {
                content_sink.flush().await?;
                bail!("streamed completion was cancelled");
            }
            async_io::CancellationResult::Completed(None) => break,
            async_io::CancellationResult::Completed(Some(chunk)) => chunk?,
        };
        tracing::trace!(chunk = ?chunk, "received stream chunk");
        if let Some(usage) = chunk.usage {
            last_usage = Some(usage);
        }
        if let Some(deltas) = response::stream_chunk_tool_call_deltas(&chunk) {
            tool_calls.push(deltas);
        }
        let (content, reasoning) = response::stream_chunk_deltas(&chunk);
        if show_reasoning && let Some(reasoning) = reasoning {
            if reasoning_inline {
                if !wrote_reasoning {
                    content_sink.write_all(b"Reasoning:\n").await?;
                }
                content_sink.write_all(reasoning.as_bytes()).await?;
                content_sink.flush().await?;
            } else {
                if !wrote_reasoning {
                    eprintln!("Reasoning:");
                }
                eprint!("{reasoning}");
            }
            wrote_reasoning = true;
        }
        if let Some(content) = content {
            if reasoning_inline && wrote_reasoning && !wrote_content {
                content_sink.write_all(b"\n\n").await?;
            }
            content_sink.write_all(content.as_bytes()).await?;
            // Only the live stdout display needs each delta pushed out
            // immediately; a `-o` file's `BufWriter` batches until the final
            // flush below instead of paying a syscall per delta.
            if reasoning_inline {
                content_sink.flush().await?;
            }
            wrote_content = true;
            content_text.push_str(content);
        }
    }

    if !wrote_content && tool_calls.is_empty() {
        bail!("API response contained no content in its first choice");
    }
    if !reasoning_inline && wrote_reasoning {
        eprintln!();
    }
    // A round that goes on to call a tool gets no trailing newline: its
    // content (if any, e.g. "Looking it up... ") is not the end of the
    // response — the next round's content is appended right after it (see
    // `RequestSettings::complete_stream`'s `append` handling), and a
    // newline in between would be a stray artifact splitting what the user
    // sees as one continuous answer. Only a round with no tool calls — the
    // actual final answer, whether reached directly or via the
    // `response_format` re-issue round — gets the newline every non-tool
    // `--stream` response has always ended with.
    if wrote_content && tool_calls.is_empty() {
        content_sink.write_all(b"\n").await?;
    }
    content_sink.flush().await?;
    Ok(StreamOutcome {
        content: content_text,
        usage: last_usage,
        tool_calls: tool_calls.finish()?,
    })
}

/// What `stream_response` produced: the full response text (concatenated
/// from every content delta, exactly as printed), the usage its final chunk
/// carried, if any, and any streamed tool calls it reassembled (empty unless
/// the round advertised tools and the model asked to call at least one —
/// see `RequestSettings::complete_stream`). The content half exists for
/// callers that need the complete text after the stream ends even though it
/// was already written out incrementally — recording a `--session` turn or a
/// `lait history` entry, neither of which can work from deltas alone.
#[derive(Debug)]
pub(crate) struct StreamOutcome {
    pub(crate) content: String,
    pub(crate) tool_calls: Vec<response::ToolCall>,
    pub(crate) usage: Option<response::Usage>,
}

/// The per-call inputs to `call_agent` beyond settings/env: the JSON-parsed
/// input (for rendering the agent's system prompt template), the raw text
/// sent as the user message, and any `--image`-style attachments for it.
/// Bundled, like `PromptTurn` above, to keep `call_agent`'s argument count
/// under clippy's `too_many_arguments` threshold.
pub(crate) struct AgentTurn<'a> {
    pub(crate) input: &'a serde_json::Value,
    pub(crate) prompt: &'a str,
    pub(crate) image_urls: &'a [String],
}

impl<'a> AgentTurn<'a> {
    /// A turn with no image attachments — every caller but `execute_step`'s
    /// agent branch, which has a node's own `images:` to resolve.
    pub(crate) fn simple(input: &'a serde_json::Value, prompt: &'a str) -> Self {
        Self {
            input,
            prompt,
            image_urls: &[],
        }
    }
}

/// Renders an agent's system prompt against `turn.input`, calls the model
/// with `turn.prompt` as the user message, and renders the response. Shared
/// by `run_agent`, `execute_step`'s agent branch, and `call_subagent_tool`.
/// `active_agent_paths` is threaded straight through to `settings.complete`
/// — see its doc comment; every caller but `call_subagent_tool` passes `&[]`.
pub(crate) async fn call_agent(
    agent_file: &AgentFile,
    settings: &RequestSettings,
    env: &AppContext,
    turn: AgentTurn<'_>,
    steps_outputs: &workflow::StepOutputs,
    active_agent_paths: &[PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let system_prompt = template::render(
        &agent_file.system_prompt_template,
        turn.input,
        steps_outputs,
        &env.vars,
    )?;
    let response_format = if agent_file.structured_output {
        Some(
            schema::build_response_format_from_entry_cancellable(
                agent_file.output_schema.as_ref().expect(
                    "load_agent validates structured_output implies output_schema is present",
                ),
                agent_file.schema_name(),
                cancellation.clone(),
            )
            .await?,
        )
    } else {
        None
    };

    let response = settings
        .complete(
            env,
            active_agent_paths,
            PromptTurn {
                system_prompt: Some(&system_prompt),
                history: &[],
                prompt: turn.prompt,
                image_urls: turn.image_urls,
            },
            response_format,
            cancellation,
        )
        .await?;
    response::render_response(&response, false, false)
}

/// The maximum recursive subagent-calling depth (a subagent whose own
/// `subagents:` names another, whose own names another, ...), rejected as a
/// runtime error the same way `MAX_WORKFLOW_DEPTH` rejects excessive
/// `workflow:` nesting.
const MAX_SUBAGENT_DEPTH: usize = 16;

/// Converts a JSON value into raw prompt/tool-input text: a `Value::String`
/// passes through unquoted (so `{{ input }}` sees the same plain text
/// everywhere else in the pipeline does), any other value (object, array,
/// number, ...) is serialized to compact JSON text. `context` names the
/// caller's own site for the serialization-failure error. Shared by
/// `run_steps`' `for_each` branch (both the sequential and concurrent
/// per-item conversion) and `subagent_tool_input` below — the same "is this
/// already plain text, or does it need to become a JSON text blob" question
/// both ask.
pub(crate) fn value_to_input_text(
    value: &serde_json::Value,
    context: &'static str,
) -> Result<String> {
    match value {
        serde_json::Value::String(text) => Ok(text.clone()),
        other => serde_json::to_string(other).context(context),
    }
}

/// Unwraps a subagent tool call's raw JSON `arguments` into the `(input,
/// prompt)` pair `call_agent` needs — the parsed JSON value for `{{
/// input.field }}` template access, and the raw text sent as the user-role
/// message. Mirrors `subagent::AgentRegistry::tools`' two parameter shapes:
/// when `file` declares an `input_schema`, the whole `arguments` object *is*
/// the subagent's input (its own schema already shaped `parameters`, so
/// there's nothing to unwrap) and `prompt` is its canonical JSON text;
/// otherwise `arguments` is the generic `{ "input": ... }` wrapper, and
/// `input`/`prompt` are read out of its `input` field via
/// `value_to_input_text`.
fn subagent_tool_input(
    file: &AgentFile,
    arguments_json: &str,
) -> Result<(serde_json::Value, String)> {
    let arguments: serde_json::Value = if arguments_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments_json)
            .context("failed to parse subagent tool call arguments as JSON")?
    };

    if file.input_schema.is_some() {
        let prompt = value_to_input_text(
            &arguments,
            "failed to serialize subagent tool call arguments",
        )?;
        return Ok((arguments, prompt));
    }

    // Moved out (not cloned) when `arguments` is an object, since `arguments`
    // itself is never used again after this.
    let input_value = match arguments {
        serde_json::Value::Object(mut map) => map.remove("input"),
        _ => None,
    }
    .ok_or_else(|| anyhow!("subagent tool call is missing the required 'input' field"))?;
    let prompt = value_to_input_text(
        &input_value,
        "failed to serialize subagent tool call 'input'",
    )?;
    Ok((input_value, prompt))
}

/// Runs subagent `name` (resolved via `env.agent_registry`, an `agents:`
/// entry) against one tool call's raw JSON `arguments`, recursively driving
/// its own completion (and, if it declares `subagents:`/`mcp:` of its own,
/// its own tool loop) to completion, and returns its rendered response text —
/// the shape a `tool`-role message needs. `active_paths` is every subagent
/// file already executing on this call stack (canonicalized); calling a
/// subagent already on it (a cycle) or beyond `MAX_SUBAGENT_DEPTH` is
/// rejected the same way `WorkflowScope`/`check_workflow_nesting` reject
/// excessive `workflow:` nesting. Boxed because this is mutually recursive
/// with `RequestSettings::complete` through `call_agent`, which Rust's
/// `async fn` cannot size otherwise.
pub(crate) fn call_subagent_tool<'a>(
    name: &'a str,
    arguments_json: &'a str,
    env: &'a AppContext,
    active_paths: &'a [PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        // `Copy` (it only captures `name: &str`), so it can back every
        // `with_context` call below without re-typing the same `format!`.
        let context = || format!("subagent '{name}'");

        let loaded = env
            .agent_registry
            .load_cancellable(name, cancellation.clone())
            .await?;

        if let Err(error) =
            nesting::check_nesting_depth(active_paths, &loaded.canonical_path, MAX_SUBAGENT_DEPTH)
        {
            match error {
                nesting::NestingDepthError::Cycle => bail!(
                    "calling subagent '{name}' would create a cycle ('{}' is already running)",
                    loaded.canonical_path.display()
                ),
                nesting::NestingDepthError::TooDeep => bail!(
                    "calling subagent '{name}' exceeded the maximum subagent nesting depth of \
                     {MAX_SUBAGENT_DEPTH}"
                ),
            }
        }

        let (input, prompt) =
            subagent_tool_input(&loaded.file, arguments_json).with_context(context)?;
        loaded.validate_input(&input).with_context(context)?;

        let settings = agent_file_settings(&loaded.file, &env.file_config, Some(name))
            .with_context(context)?
            .with_usage_label(format!("subagent '{name}'"));

        let mut next_active_paths = active_paths.to_vec();
        next_active_paths.push(loaded.canonical_path.clone());

        call_agent(
            &loaded.file,
            &settings,
            env,
            AgentTurn::simple(&input, &prompt),
            &workflow::StepOutputs::new(),
            &next_active_paths,
            cancellation,
        )
        .await
        .with_context(context)
    })
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
    base_url_override: Option<String>,
    api_key_override: Option<String>,
    capability_overrides: CapabilityOverrides,
    local_models: &ModelMap,
    file_config: &ConfigFile,
) -> Result<RequestSettings> {
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
    let (base_url, api_key) = config::resolve_endpoint(
        base_url_override,
        api_key_override,
        resolved_model.base_url.as_deref(),
        resolved_model.api_key.as_deref(),
        resolved_model.api_key_cmd.as_ref(),
        file_config,
    )?;
    let api_key = api_key.unwrap_or_else(|| {
        // async-openai always builds an Authorization header from its config.
        // LM Studio ignores the value, so use a non-empty dummy key when no
        // key was supplied instead of making local requests fail on an empty
        // header value.
        "lm-studio".to_owned()
    });
    let sampling = SamplingOverrides {
        reasoning_effort: overrides
            .reasoning_effort
            .or(resolved_model.reasoning_effort)
            .or(file_config.default.reasoning_effort),
        temperature: overrides
            .temperature
            .or(resolved_model.temperature)
            .or(file_config.default.temperature),
        top_p: overrides
            .top_p
            .or(resolved_model.top_p)
            .or(file_config.default.top_p),
        max_tokens: overrides
            .max_tokens
            .or(resolved_model.max_tokens)
            .or(file_config.default.max_tokens),
    };
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

    let mcp = capability_overrides
        .mcp
        .or_else(|| file_config.default.mcp.clone())
        .unwrap_or_default();
    let max_tool_rounds = capability_overrides
        .max_tool_rounds
        .or(file_config.default.max_tool_rounds);
    llm::validate_max_tool_rounds(max_tool_rounds, &request_context)?;
    let max_tool_rounds = max_tool_rounds.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);
    let skills = capability_overrides
        .skills
        .or_else(|| file_config.default.skills.clone())
        .unwrap_or_default();
    let subagents = capability_overrides
        .subagents
        .or_else(|| file_config.default.subagents.clone())
        .unwrap_or_default();
    let tools = capability_overrides
        .tools
        .or_else(|| file_config.default.tools.clone())
        .unwrap_or_default();

    tracing::debug!(
        model_id = %resolved_model.model_id,
        base_url = %base_url,
        api_key = %crate::logging::mask_secret(&api_key),
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
        None,
        None,
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
    use super::stream_response;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

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
        assert!(error.to_string().contains("cancelled"), "{error}");
    }
}
