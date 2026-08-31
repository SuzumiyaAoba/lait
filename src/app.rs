use std::{
    borrow::Cow,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat,
};
use futures_util::{StreamExt, TryStreamExt};

use crate::{
    agent::{self, AgentFile},
    async_io, attachment,
    cli::{AgentAction, ChatArgs, ChatReplArgs, Cli, Command, LintArgs, RunArgs},
    cli::{AgentRunArgs, PromptArgs, ReasoningEffort, SharedChatArgs},
    config::{self, ConfigFile, ModelMap},
    docgen, history, jq, lint, llm, mcp, prompt, render, repl, response, schema, session, skill,
    subagent, template, usage, workflow,
};

pub(crate) const DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";

/// Reads all of stdin into a string, trimming trailing newlines (piped text
/// almost always ends in one, and a prompt should not).
fn read_stdin_text() -> Result<String> {
    use std::io::Read;

    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .context("failed to read stdin")?;
    Ok(buffer.trim_end_matches(['\n', '\r']).to_owned())
}

/// Combines a positional PROMPT/INPUT argument with piped stdin, the shared
/// rule for chat, `lait run`, and `lait agent run`: a `-` argument reads
/// stdin as the whole input; otherwise piped stdin (stdin not being a TTY)
/// is the whole input when no argument is given, or is appended to the
/// argument as context when one is. Returns `Ok(None)` when there is no
/// input from either source — each caller reports that with its own message.
fn resolve_input_with_stdin(positional: Option<String>) -> Result<Option<String>> {
    use std::io::IsTerminal;

    if positional.as_deref() == Some("-") {
        return Ok(Some(read_stdin_text()?).filter(|text| !text.trim().is_empty()));
    }
    let piped_text = if std::io::stdin().is_terminal() {
        None
    } else {
        // An empty pipe (e.g. `< /dev/null`) counts as no input at all, not
        // as an empty prompt.
        Some(read_stdin_text()?).filter(|text| !text.trim().is_empty())
    };
    Ok(match (positional, piped_text) {
        // Both given: the instruction first, then the piped text as context,
        // separated by a blank line (e.g. `git diff | lait "review this"`).
        (Some(argument), Some(piped)) => Some(format!("{argument}\n\n{piped}")),
        (Some(argument), None) => Some(argument),
        (None, piped) => piped,
    })
}

/// Records a completed chat/agent/workflow/prompt run in `lait history`,
/// unless `no_history` (the caller's own `--no-history`) or
/// `default.history: false` opts out — the one gate every `run_*` entry
/// point goes through before ever calling `history::record`, so recording
/// can never happen from a place that forgot to check the opt-out. Called
/// only after a run has actually succeeded (every call site is on the
/// success path), matching `history::record`'s own contract.
fn record_history(
    no_history: bool,
    file_config: &ConfigFile,
    kind: &str,
    model: Option<&str>,
    prompt: &str,
    response: &str,
    usage: Option<response::Usage>,
) -> Result<()> {
    if no_history || !file_config.default.history.unwrap_or(true) {
        return Ok(());
    }
    history::record(kind, model, prompt, response, usage)
}

/// Records one finished chat turn: appends it to `--session`'s log (when
/// set) and to `lait history` (unless suppressed) — the shared tail of
/// `run_chat`'s streamed and non-streamed paths and `run_chat_repl`'s
/// per-turn loop.
fn finish_chat_turn(
    session_name: Option<&str>,
    no_history: bool,
    file_config: &ConfigFile,
    model_id: &str,
    prompt: &str,
    response: &str,
    usage: Option<response::Usage>,
) -> Result<()> {
    if let Some(name) = session_name {
        session::append_turn(name, prompt, response)?;
    }
    record_history(
        no_history,
        file_config,
        "chat",
        Some(model_id),
        prompt,
        response,
        usage,
    )
}

/// The maximum number of tool-call round trips a single completion request
/// may take (see `RequestSettings::complete`) before lait gives up and
/// errors instead of looping forever on a model that keeps calling tools.
/// Overridable per CLI invocation/agent file/workflow node/`default:` via
/// `max_tool_rounds`.
const DEFAULT_MAX_TOOL_ROUNDS: usize = 8;

pub(crate) async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Run(run_args)) => run_workflow(run_args, cli.no_config).await,
        Some(Command::Agent(agent_command)) => match agent_command.action {
            AgentAction::Run(args) => run_agent(args, cli.no_config).await,
        },
        Some(Command::Lint(lint_args)) => lint_files(lint_args, cli.no_config),
        Some(Command::Models(models_args)) => crate::models::run(models_args, cli.no_config).await,
        Some(Command::Completions(completions_args)) => {
            docgen::generate_completions(completions_args);
            Ok(())
        }
        Some(Command::Man(man_args)) => docgen::generate_man_pages(man_args),
        Some(Command::Init(init_args)) => crate::init::run(init_args),
        Some(Command::Sessions(sessions_command)) => crate::session::run(sessions_command),
        Some(Command::Chat(chat_repl_args)) => run_chat_repl(chat_repl_args, cli.no_config).await,
        Some(Command::Prompt(prompt_args)) => run_prompt(prompt_args, cli.no_config).await,
        Some(Command::History(history_args)) => history::run(history_args),
        None => run_chat_or_repl(cli.chat, cli.no_config).await,
    }
}

/// The bare-invocation entry point (`lait [OPTIONS] [PROMPT]`, no
/// subcommand): sends a single-shot chat request when a prompt is available
/// (an argument or piped stdin — see `resolve_input_with_stdin`), or, when
/// none is and stdin is an interactive terminal, starts the same REPL
/// `lait chat` does instead of erroring. Piped-but-empty stdin (a script's
/// `< /dev/null`, or a forgotten argument in a pipeline) still errors exactly
/// as before — only an actual interactive terminal with nothing typed counts
/// as "the user wants the REPL", so a script's exit-code contract never
/// silently changes into "launched an interactive prompt that then exits
/// immediately."
async fn run_chat_or_repl(chat: ChatArgs, no_config: bool) -> Result<()> {
    use std::io::IsTerminal;

    match resolve_input_with_stdin(chat.prompt.clone())? {
        Some(prompt) => run_chat(chat, prompt, no_config).await,
        None if std::io::stdin().is_terminal() => {
            run_chat_repl(
                ChatReplArgs {
                    shared: chat.shared,
                },
                no_config,
            )
            .await
        }
        None => Err(anyhow!(
            "a PROMPT is required; provide one, pipe input via stdin, or use `lait run <FILE> <PROMPT>`"
        )),
    }
}

/// Whether `cli`'s command awaits anything (a model request, MCP). `main`
/// consults this before building the tokio runtime, so the purely local
/// subcommands — `completions` in particular, which shell startup files run
/// on every new shell — skip spawning worker threads and go through
/// `run_blocking` instead. Every command still works through `run`, so a
/// drift in this classification costs only startup time, never correctness.
pub(crate) fn needs_async_runtime(cli: &Cli) -> bool {
    match &cli.command {
        Some(
            Command::Lint(_)
            | Command::Completions(_)
            | Command::Man(_)
            | Command::Init(_)
            | Command::Sessions(_)
            | Command::History(_),
        ) => false,
        Some(Command::Models(models_args)) => models_args.remote,
        Some(Command::Prompt(prompt_args)) => prompt_args.name != "list",
        Some(Command::Run(_) | Command::Agent(_) | Command::Chat(_)) | None => true,
    }
}

/// Runs the commands `needs_async_runtime` classifies as synchronous,
/// without any async runtime behind them.
pub(crate) fn run_blocking(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Lint(lint_args)) => lint_files(lint_args, cli.no_config),
        Some(Command::Models(models_args)) => {
            if models_args.remote {
                bail!("internal error: `models --remote` must run on the async path");
            }
            crate::models::run_local(models_args, cli.no_config)
        }
        Some(Command::Completions(completions_args)) => {
            docgen::generate_completions(completions_args);
            Ok(())
        }
        Some(Command::Man(man_args)) => docgen::generate_man_pages(man_args),
        Some(Command::Init(init_args)) => crate::init::run(init_args),
        Some(Command::Sessions(sessions_command)) => crate::session::run(sessions_command),
        Some(Command::Prompt(prompt_args)) => {
            if prompt_args.name != "list" {
                bail!("internal error: `prompt <NAME>` must run on the async path");
            }
            crate::prompt::list(&config::load_config(cli.no_config)?)
        }
        Some(Command::History(history_args)) => crate::history::run(history_args),
        Some(Command::Run(_) | Command::Agent(_) | Command::Chat(_)) | None => {
            bail!("internal error: an async command reached run_blocking")
        }
    }
}

/// Statically checks every file in `lint_args.files` (see `lint::lint_file`)
/// and prints a per-file report to stdout. Runs synchronously — every check
/// is a local file read/parse, none of it needs the async runtime `run`
/// otherwise sets up for a model request. Unlike `run_workflow`/`run_agent`,
/// one bad file doesn't stop the rest: every file is linted and reported
/// before this returns `Err` (which only happens if at least one file has an
/// `Error`-level issue, so CI can rely on the exit code).
fn lint_files(lint_args: LintArgs, no_config: bool) -> Result<()> {
    // Unlike `config::load_config`, which returns an empty `ConfigFile` both
    // when `lait.config.yml` is absent and when `--no-config` was passed,
    // the linter needs to tell "absent/skipped" apart from "present but
    // empty" so it can skip `mcp:`/`skills:` name checks (and say why)
    // instead of reporting every referenced name as unknown.
    let config_present = !no_config && Path::new(config::CONFIG_FILE_NAME).exists();
    let file_config = config::load_config(no_config)?;
    let config = config_present.then_some(&file_config);

    let mut failed_files = 0usize;
    for file in &lint_args.files {
        // `lint::lint_file` only ever returns `Err` for a file whose type it
        // can't determine (an unrecognized extension) — treated here as one
        // more failure to report, not a reason to stop linting the rest of
        // `lint_args.files`.
        let report = match lint::lint_file(file, config) {
            Ok(report) => report,
            Err(error) => {
                println!("{}:", file.display());
                println!("  error: {error:#}");
                failed_files += 1;
                continue;
            }
        };
        if report.issues.is_empty() {
            println!("{}: OK", report.file.display());
            continue;
        }
        println!("{}:", report.file.display());
        for issue in &report.issues {
            println!("  {}: {}", issue.severity, issue.message);
        }
        if report.has_errors() {
            failed_files += 1;
        }
    }

    if failed_files > 0 {
        bail!(
            "{failed_files} of {} file(s) had errors",
            lint_args.files.len()
        );
    }
    Ok(())
}

/// The reasoning-effort/temperature/top_p/max_tokens knobs a caller (CLI
/// invocation, agent file, or workflow step) may set for a single completion
/// request. Bundled into one struct (rather than four positional parameters)
/// because every layer of `resolve_request_settings`'s fallback chain treats
/// them identically: each field falls back independently to the next layer,
/// unlike e.g. `workflow::RetryDefinition`, which falls back as a whole unit.
#[derive(Debug, Default, Clone, Copy)]
struct SamplingOverrides {
    reasoning_effort: Option<ReasoningEffort>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<u32>,
}

/// The `mcp`/`max_tool_rounds`/`skills`/`subagents` knobs a caller may set
/// for a single completion request, bundled the same way as
/// `SamplingOverrides` and for the same reason (keeps
/// `resolve_request_settings`'s argument count down; each field falls back
/// independently to `file_config.default`, not as a whole unit).
#[derive(Debug, Default, Clone)]
struct CapabilityOverrides {
    mcp: Option<Vec<String>>,
    max_tool_rounds: Option<usize>,
    skills: Option<Vec<String>>,
    subagents: Option<Vec<String>>,
}

/// The new-turn inputs shared by `RequestSettings::complete`/
/// `complete_stream`: the system prompt, any prior turns from a resumed
/// `--session` (empty for every caller but chat), the new user-role prompt
/// text, and any `--image` attachments for it (empty for every caller but
/// chat). Bundled into one struct, like `SamplingOverrides`/
/// `CapabilityOverrides` above, to keep `complete`'s argument count under
/// clippy's `too_many_arguments` threshold.
struct PromptTurn<'a> {
    system_prompt: Option<&'a str>,
    history: &'a [ChatCompletionRequestMessage],
    prompt: &'a str,
    image_urls: &'a [String],
}

impl<'a> PromptTurn<'a> {
    /// A turn with no prior history and no image attachments — every caller
    /// but chat's own (`run_chat`/`run_repl_turn`, which have a real
    /// `--session`/`--image` history to carry).
    fn simple(system_prompt: Option<&'a str>, prompt: &'a str) -> Self {
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
struct RequestSettings {
    base_url: String,
    api_key: String,
    resolved_model: config::ResolvedModel,
    sampling: SamplingOverrides,
    /// Names of `mcp_servers:` entries whose tools this request may call.
    /// Empty means "no tools" — `complete`'s fast path then behaves exactly
    /// like a single-shot request always has.
    mcp: Vec<String>,
    max_tool_rounds: usize,
    /// Names of `skills:` entries whose content is appended to this
    /// request's system prompt (see `with_skills`). Empty means no skill
    /// content is appended.
    skills: Vec<String>,
    /// Names of `agents:` entries made available as callable subagent tools
    /// during this request's tool loop. Empty means "no subagent tools" —
    /// combined with `mcp` the same way in `complete`'s tool loop (empty
    /// tool sources for both keeps `complete`'s fast, tool-free path).
    subagents: Vec<String>,
    /// Names these settings' requests in `env.usage`'s `--show-usage`
    /// summary (a step label, an agent name, `"chat"`); every round of a
    /// tool loop records under the same label. Set via `with_usage_label`
    /// right after resolving, where the caller still knows what it is
    /// resolving for.
    usage_label: String,
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

impl RequestSettings {
    /// Sets `usage_label` — see that field's doc comment.
    fn with_usage_label(mut self, label: impl Into<String>) -> Self {
        self.usage_label = label.into();
        self
    }

    /// Builds an `llm::CompletionRequest` from these settings plus the
    /// per-call `response_format`/`messages`/`tools`. The `base_url`/
    /// `api_key`/`model_id`/sampling fields are the same for every request
    /// `self` ever builds, so both `complete`'s tool loop and
    /// `complete_stream` go through here instead of repeating that field
    /// list at each call site.
    fn request<'a>(
        &'a self,
        response_format: Option<ResponseFormat>,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: &'a [ChatCompletionTools],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> llm::CompletionRequest<'a> {
        llm::CompletionRequest {
            base_url: &self.base_url,
            api_key: &self.api_key,
            model_id: &self.resolved_model.model_id,
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
    async fn complete(
        &self,
        env: &AppContext<'_>,
        active_agent_paths: &[PathBuf],
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<response::ChatCompletionResponse> {
        let system_prompt = self
            .system_prompt_with_skills(&env.skill_cache, turn.system_prompt, cancellation.clone())
            .await?;
        let system_prompt = system_prompt.as_deref();

        if self.mcp.is_empty() && self.subagents.is_empty() {
            let messages =
                llm::initial_messages(system_prompt, turn.history, turn.prompt, turn.image_urls)?;
            return self
                .complete_recorded(env, response_format, messages, &[], cancellation)
                .await;
        }

        // `agent_registry.tools` is synchronous (it only reads local subagent
        // files), so joining it with the MCP round trip lets both proceed
        // together instead of paying the MCP latency before ever touching
        // disk.
        let (mut mcp_tool_set, mut subagent_tool_set) = tokio::try_join!(
            env.registry.tools(&self.mcp, cancellation.clone()),
            env.agent_registry
                .tools_cancellable(&self.subagents, cancellation.clone()),
        )?;
        for name in subagent_tool_set.names() {
            if mcp_tool_set.contains(name) {
                bail!("tool name collision: an MCP tool and a subagent both qualify to '{name}'");
            }
        }
        // Only `.contains()`/`.subagent_name()` (which read `.index`, not
        // `.tools`) are used below, so `.tools` doesn't need to survive past
        // this merge — moving it out avoids cloning every tool definition
        // (including its full JSON `parameters`).
        let mut tools = std::mem::take(&mut mcp_tool_set.tools);
        tools.extend(std::mem::take(&mut subagent_tool_set.tools));

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
            messages.push(llm::assistant_tool_call_message(tool_calls, content)?);

            // A model turn's `tool_calls` are independent by construction (it
            // couldn't have seen one call's result before deciding on
            // another), so they're run concurrently rather than one at a
            // time; `try_join_all` preserves `tool_calls`' order regardless
            // of completion order, so the appended `tool`-role messages stay
            // in a stable, deterministic order.
            let tool_messages =
                futures_util::future::try_join_all(tool_calls.iter().map(|tool_call| async {
                    let name = &tool_call.function.name;
                    let result = if mcp_tool_set.contains(name) {
                        env.registry
                            .call(
                                &mcp_tool_set,
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
                    } else {
                        bail!("model called unknown tool '{name}'");
                    };
                    llm::tool_result_message(&tool_call.id, result)
                }))
                .await?;
            messages.extend(tool_messages);
        }
    }

    /// The one way `complete` sends a request: builds it via `request`,
    /// awaits it, and records the response's usage under
    /// `self.usage_label` — so no future call site can forget the recording
    /// and skew `--show-usage`.
    async fn complete_recorded(
        &self,
        env: &AppContext<'_>,
        response_format: Option<ResponseFormat>,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: &[ChatCompletionTools],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<response::ChatCompletionResponse> {
        let response =
            llm::complete(self.request(response_format, messages, tools, cancellation)).await?;
        env.usage.record_response(&self.usage_label, &response);
        Ok(response)
    }

    /// Like [`RequestSettings::complete`], but requests a streamed response.
    /// Rejects `self.mcp`/`self.subagents` being non-empty: a streamed
    /// `tool_calls` field arrives as index-keyed fragments that must be
    /// reassembled before they can be routed to an MCP server or a subagent,
    /// which lait does not yet do (see `docs/usage/ja/mcp.md`). `self.skills`
    /// is appended to `system_prompt` the same way as in `complete` — skills
    /// are static injection, so unlike `mcp`/`subagents` they impose no such
    /// restriction on streaming.
    /// `include_usage` asks the server for a final usage chunk (see
    /// `llm::CompletionRequest::stream_include_usage`); set it only when the
    /// caller will actually display it (`--show-usage`). `turn.history`/
    /// `turn.image_urls` behave exactly as in `complete` — see its doc comment.
    async fn complete_stream(
        &self,
        skill_cache: &skill::SkillCache<'_>,
        turn: PromptTurn<'_>,
        response_format: Option<ResponseFormat>,
        include_usage: bool,
    ) -> Result<llm::CompletionStream> {
        if !self.mcp.is_empty() {
            bail!(
                "'--stream'/streaming is not supported together with 'mcp:' yet; drop one of them"
            );
        }
        if !self.subagents.is_empty() {
            bail!(
                "'--stream'/streaming is not supported together with 'subagents:' yet; drop one \
                 of them"
            );
        }
        let system_prompt = self
            .system_prompt_with_skills(skill_cache, turn.system_prompt, None)
            .await?;
        let messages = llm::initial_messages(
            system_prompt.as_deref(),
            turn.history,
            turn.prompt,
            turn.image_urls,
        )?;
        let mut request = self.request(response_format, messages, &[], None);
        request.stream_include_usage = include_usage;
        llm::complete_stream(request).await
    }

    /// Shared by `complete`/`complete_stream`: resolves `self.skills` against
    /// `skill_cache` and appends the result to `system_prompt` — see
    /// `with_skills`.
    async fn system_prompt_with_skills<'a>(
        &self,
        skill_cache: &skill::SkillCache<'_>,
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
/// path. Fails, like `response::response_content`, if the stream ends
/// without ever producing content.
/// Returns the accumulated content text (for `--session`/`lait history` to
/// record — see `StreamOutcome`) alongside the usage carried by the final
/// chunk, when the request asked for one (see
/// `RequestSettings::complete_stream`'s `include_usage`) and the server
/// obliged. `output_path` redirects the content to a file (`-o`): the file
/// then holds the body alone, so reasoning deltas — normally written ahead of
/// the content on stdout — go to stderr instead.
async fn stream_response(
    mut stream: llm::CompletionStream,
    show_reasoning: bool,
    output_path: Option<&Path>,
) -> Result<StreamOutcome> {
    use std::io::Write;

    // Locked/opened once for the stream's whole lifetime: every delta write
    // below would otherwise re-acquire stdout's mutex, and nothing else
    // prints to the content sink while a response is streaming.
    let mut stdout_lock;
    let mut file_writer;
    // Whether reasoning shares the content sink (the stdout presentation:
    // a `Reasoning:` header, then a blank line before the content).
    let reasoning_inline = output_path.is_none();
    let content_sink: &mut dyn Write = match output_path {
        None => {
            stdout_lock = std::io::stdout().lock();
            &mut stdout_lock
        }
        Some(path) => {
            let file = std::fs::File::create(path)
                .with_context(|| format!("failed to create output file '{}'", path.display()))?;
            file_writer = std::io::BufWriter::new(file);
            &mut file_writer
        }
    };
    let mut wrote_reasoning = false;
    let mut wrote_content = false;
    let mut last_usage = None;
    let mut content_text = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(usage) = chunk.usage {
            last_usage = Some(usage);
        }
        let (content, reasoning) = response::stream_chunk_deltas(&chunk);
        if show_reasoning && let Some(reasoning) = reasoning {
            if reasoning_inline {
                if !wrote_reasoning {
                    writeln!(content_sink, "Reasoning:")?;
                }
                write!(content_sink, "{reasoning}")?;
                content_sink.flush()?;
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
                write!(content_sink, "\n\n")?;
            }
            write!(content_sink, "{content}")?;
            // Only the live stdout display needs each delta pushed out
            // immediately; a `-o` file's `BufWriter` batches until the final
            // flush below instead of paying a syscall per delta.
            if reasoning_inline {
                content_sink.flush()?;
            }
            wrote_content = true;
            content_text.push_str(content);
        }
    }

    if !wrote_content {
        bail!("API response contained no content in its first choice");
    }
    if !reasoning_inline && wrote_reasoning {
        eprintln!();
    }
    writeln!(content_sink)?;
    content_sink.flush()?;
    Ok(StreamOutcome {
        content: content_text,
        usage: last_usage,
    })
}

/// What `stream_response` produced: the full response text (concatenated
/// from every content delta, exactly as printed) and the usage its final
/// chunk carried, if any. The content half exists for callers that need the
/// complete text after the stream ends even though it was already written
/// out incrementally — recording a `--session` turn or a `lait history`
/// entry, neither of which can work from deltas alone.
struct StreamOutcome {
    content: String,
    usage: Option<response::Usage>,
}

/// The per-call inputs to `call_agent` beyond settings/env: the JSON-parsed
/// input (for rendering the agent's system prompt template), the raw text
/// sent as the user message, and any `--image`-style attachments for it.
/// Bundled, like `PromptTurn` above, to keep `call_agent`'s argument count
/// under clippy's `too_many_arguments` threshold.
struct AgentTurn<'a> {
    input: &'a serde_json::Value,
    prompt: &'a str,
    image_urls: &'a [String],
}

impl<'a> AgentTurn<'a> {
    /// A turn with no image attachments — every caller but `execute_step`'s
    /// agent branch, which has a node's own `images:` to resolve.
    fn simple(input: &'a serde_json::Value, prompt: &'a str) -> Self {
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
async fn call_agent(
    agent_file: &AgentFile,
    settings: &RequestSettings,
    env: &AppContext<'_>,
    turn: AgentTurn<'_>,
    steps_outputs: &workflow::StepOutputs,
    active_agent_paths: &[PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let system_prompt = template::render(
        &agent_file.system_prompt_template,
        turn.input,
        steps_outputs,
        &serde_json::Map::new(),
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
fn value_to_input_text(value: &serde_json::Value, context: &'static str) -> Result<String> {
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
fn call_subagent_tool<'a>(
    name: &'a str,
    arguments_json: &'a str,
    env: &'a AppContext<'a>,
    active_paths: &'a [PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Pin<Box<dyn Future<Output = Result<String>> + 'a>> {
    Box::pin(async move {
        // `Copy` (it only captures `name: &str`), so it can back every
        // `with_context` call below without re-typing the same `format!`.
        let context = || format!("subagent '{name}'");

        let loaded = env
            .agent_registry
            .load_cancellable(name, cancellation.clone())
            .await?;

        if let Err(error) =
            check_nesting_depth(active_paths, &loaded.canonical_path, MAX_SUBAGENT_DEPTH)
        {
            match error {
                NestingDepthError::Cycle => bail!(
                    "calling subagent '{name}' would create a cycle ('{}' is already running)",
                    loaded.canonical_path.display()
                ),
                NestingDepthError::TooDeep => bail!(
                    "calling subagent '{name}' exceeded the maximum subagent nesting depth of \
                     {MAX_SUBAGENT_DEPTH}"
                ),
            }
        }

        let (input, prompt) =
            subagent_tool_input(&loaded.file, arguments_json).with_context(context)?;
        loaded.validate_input(&input).with_context(context)?;

        let settings = agent_file_settings(&loaded.file, env.file_config, Some(name))
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
fn resolve_request_settings(
    model_name: String,
    overrides: SamplingOverrides,
    base_url_override: Option<String>,
    api_key_override: Option<String>,
    capability_overrides: CapabilityOverrides,
    local_models: &ModelMap,
    file_config: &ConfigFile,
) -> Result<RequestSettings> {
    let resolved_model = match config::resolve_model_alias(&model_name, local_models)? {
        Some(resolved) => resolved,
        None => config::resolve_model(model_name, file_config)?,
    };
    let (base_url, api_key) = resolve_endpoint(
        base_url_override,
        api_key_override,
        resolved_model.base_url.as_deref(),
        resolved_model.api_key.as_deref(),
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

    Ok(RequestSettings {
        base_url,
        api_key,
        resolved_model,
        sampling,
        mcp,
        max_tool_rounds,
        skills,
        subagents,
        usage_label: String::new(),
    })
}

/// Resolves the endpoint a request goes to from the three layers every
/// caller shares — explicit override > model-definition value > config
/// top-level — falling back to `DEFAULT_BASE_URL`, normalizing the trailing
/// slash, and rejecting an empty base URL. `${VAR}` placeholders are only
/// expanded in the config-sourced layers (see
/// `config::expand_env_placeholders`), never in an override, which the
/// shell already expands on its own. The API key comes back as `None` when
/// no layer sets one — `resolve_request_settings` substitutes its dummy
/// key, `lait models --remote` sends no Authorization header at all.
pub(crate) fn resolve_endpoint(
    base_url_override: Option<String>,
    api_key_override: Option<String>,
    model_base_url: Option<&str>,
    model_api_key: Option<&str>,
    file_config: &ConfigFile,
) -> Result<(String, Option<String>)> {
    let model_base_url = model_base_url
        .map(config::expand_env_placeholders)
        .transpose()?;
    let config_base_url = file_config
        .base_url
        .as_deref()
        .map(config::expand_env_placeholders)
        .transpose()?;
    let base_url = base_url_override
        .or(model_base_url)
        .or(config_base_url)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let base_url = base_url.trim_end_matches('/').to_owned();
    if base_url.is_empty() {
        return Err(anyhow!("base URL must not be empty"));
    }
    let model_api_key = model_api_key
        .map(config::expand_env_placeholders)
        .transpose()?;
    let config_api_key = file_config
        .api_key
        .as_deref()
        .map(config::expand_env_placeholders)
        .transpose()?;
    let api_key = api_key_override.or(model_api_key).or(config_api_key);
    Ok((base_url, api_key))
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
fn agent_file_settings(
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
        },
        &ModelMap::default(),
        file_config,
    )
}

/// Resolves chat mode's system prompt: `--system` text, else `--system-file`
/// contents, else `default.system` from lait.config.yml (`--system` and
/// `--system-file` conflict at the clap level, so their order here never
/// actually decides anything).
fn resolve_system_prompt(
    shared: &SharedChatArgs,
    file_config: &ConfigFile,
) -> Result<Option<String>> {
    if let Some(text) = &shared.system {
        return Ok(Some(text.clone()));
    }
    if let Some(path) = &shared.system_file {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read system prompt file '{}'", path.display()))?;
        return Ok(Some(text.trim_end().to_owned()));
    }
    Ok(file_config.default.system.clone())
}

/// Resolves a chat turn's `RequestSettings` from `shared` (the options common
/// to single-shot chat and `lait chat`'s REPL — see `SharedChatArgs`) and
/// `file_config`. Shared by `run_chat` and `repl::run`, which both need
/// exactly this: chat's own model-resolution rule (`--model`/`LLM_MODEL` >
/// `prompt_model_fallback` > `default.model`) plus the sampling/capability
/// overrides every chat turn carries. The REPL calls this again after
/// `/model`, so a model switch re-resolves the full settings (base URL,
/// sampling defaults, ...) rather than only swapping the model id.
/// `prompt_model_fallback` is `-p`/`--prompt-name`'s own `model:`, when set
/// and `-p` was used (`None` from every other caller, including the REPL,
/// which has no `-p` equivalent).
fn resolve_chat_settings(
    shared: &SharedChatArgs,
    prompt_model_fallback: Option<&str>,
    file_config: &ConfigFile,
) -> Result<RequestSettings> {
    let model_name = shared
        .model
        .clone()
        .or_else(|| prompt_model_fallback.map(str::to_owned))
        .or_else(|| file_config.default.model.clone())
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "model is required; provide --model, set LLM_MODEL, or specify default.model in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
    let settings = resolve_request_settings(
        model_name,
        SamplingOverrides {
            reasoning_effort: shared.reasoning_effort,
            temperature: shared.temperature,
            top_p: shared.top_p,
            max_tokens: shared.max_tokens,
        },
        shared.base_url.clone(),
        shared.api_key.clone(),
        CapabilityOverrides {
            mcp: (!shared.mcp.is_empty()).then(|| shared.mcp.clone()),
            max_tool_rounds: None,
            // No `--skill` CLI flag: chat only ever gets skills from
            // `default.skills` in `lait.config.yml` (see `resolve_request_settings`).
            skills: None,
            subagents: (!shared.subagent.is_empty()).then(|| shared.subagent.clone()),
        },
        &ModelMap::default(),
        file_config,
    )?;
    Ok(settings.with_usage_label("chat"))
}

/// Resolves `shared.session`'s prior turns (empty when `--session` is unset)
/// into the shape `PromptTurn::history` needs. Shared by `run_chat` and
/// `repl::run`'s startup (the REPL loads history once and grows its own
/// in-memory copy turn by turn from there, rather than reloading from disk
/// every turn).
fn load_session_history(session_name: Option<&str>) -> Result<Vec<ChatCompletionRequestMessage>> {
    match session_name {
        Some(name) => session::to_request_messages(&session::load(name)?),
        None => Ok(Vec::new()),
    }
}

/// Runs a single-shot chat request with an already-resolved `prompt` — see
/// `run_chat_or_repl`, the only caller, for how `prompt` was resolved (a
/// CLI argument and/or piped stdin).
async fn run_chat(chat: ChatArgs, prompt: String, no_config: bool) -> Result<()> {
    let file_config = config::load_config(no_config)?;

    // `-p`/`--prompt-name` renders a named `prompts:` template against
    // `prompt` (which, for this path, is really the template's `{{ input }}`
    // rather than literal text to send) before anything else touches it —
    // `--file` attachments below still append to the *rendered* text, the
    // same way they'd append to a plain prompt.
    let (prompt, prompt_model_fallback) = match &chat.prompt_name {
        Some(name) => prompt::render_named(name, &prompt, &chat.var, &file_config)?,
        None => (prompt, None),
    };
    let prompt = match attachment::read_file_attachments(&chat.files).await? {
        Some(file_context) => format!("{prompt}\n\n{file_context}"),
        None => prompt,
    };

    let settings =
        resolve_chat_settings(&chat.shared, prompt_model_fallback.as_deref(), &file_config)?;

    let response_format = chat
        .json_schema
        .as_deref()
        .map(|path| schema::load_json_schema(path, &chat.schema_name))
        .transpose()?;

    let system_prompt = resolve_system_prompt(&chat.shared, &file_config)?;
    let image_urls = attachment::resolve_image_urls(&chat.images).await?;
    let session_history = load_session_history(chat.shared.session.as_deref())?;
    let env = AppContext::new(&file_config);

    // `--quiet` keeps the response body and drops every note around it.
    let show_reasoning = chat.shared.show_reasoning && !chat.quiet;
    let show_usage = chat.shared.show_usage && !chat.quiet;
    let render_enabled = chat.render || file_config.default.render.unwrap_or(false);
    // `-o -` is an explicit "stdout", the same as no `-o` at all.
    let output_path = chat
        .output
        .as_deref()
        .filter(|path| path.as_os_str() != "-");

    let turn = PromptTurn {
        system_prompt: system_prompt.as_deref(),
        history: &session_history,
        prompt: &prompt,
        image_urls: &image_urls,
    };

    if chat.stream {
        let outcome = env
            .finish(async {
                let stream = settings
                    .complete_stream(&env.skill_cache, turn, response_format, show_usage)
                    .await?;
                stream_response(stream, show_reasoning, output_path).await
            })
            .await?;
        // Streamed usage arrives on the final chunk rather than through
        // `complete`; feed it into the same tally so both chat paths share
        // one summary format and so `env.usage.total()` below reflects it.
        if let Some(usage) = outcome.usage {
            env.usage.record(&settings.usage_label, usage);
        }
        finish_chat_turn(
            chat.shared.session.as_deref(),
            chat.shared.no_history,
            &file_config,
            &settings.resolved_model.model_id,
            &prompt,
            &outcome.content,
            env.usage.total(),
        )?;
        if show_usage {
            usage::print_usage_summary(&env.usage);
        }
        return Ok(());
    }

    let response = env
        .finish(settings.complete(&env, &[], turn, response_format, env.cancel.clone()))
        .await?;

    match output_path {
        Some(path) => {
            // The file gets the body alone; reasoning, when requested,
            // becomes a stderr note like usage.
            if show_reasoning && let Some(reasoning) = response::response_reasoning(&response) {
                eprintln!("Reasoning:\n{reasoning}\n");
            }
            let mut body = response::render_response(&response, chat.json, false)?;
            body.push('\n');
            std::fs::write(path, body)
                .with_context(|| format!("failed to write the response to '{}'", path.display()))?;
        }
        None => {
            let output = response::render_response(&response, chat.json, show_reasoning)?;
            // `--json`'s output is machine-readable and never rendered as
            // Markdown; `chat.stream`'s branch above already returned before
            // reaching here, so `--render` never has to reckon with a
            // partial streamed response either — see `render::maybe_render`.
            let output = if chat.json {
                output
            } else {
                render::maybe_render(&output, render_enabled)
            };
            println!("{output}");
        }
    }
    let content = response::content_text(&response);
    finish_chat_turn(
        chat.shared.session.as_deref(),
        chat.shared.no_history,
        &file_config,
        &settings.resolved_model.model_id,
        &prompt,
        content,
        env.usage.total(),
    )?;
    if show_usage {
        usage::print_usage_summary(&env.usage);
    }
    Ok(())
}

/// Runs `lait chat`'s interactive REPL: reads one line at a time from stdin,
/// sends it (plus every earlier turn this process has seen) to the model,
/// and prints the reply, until `/exit` or end-of-input (Ctrl-D closes stdin,
/// which a piped-stdin test also relies on to end the loop without an
/// explicit `/exit`). See `repl::parse_meta_command` for the `/exit`/
/// `/clear`/`/model`/`/system` syntax handled below. Also reached from a
/// prompt-less, stdin-is-a-terminal bare `lait` invocation — see
/// `run_chat_or_repl`.
async fn run_chat_repl(args: ChatReplArgs, no_config: bool) -> Result<()> {
    use std::io::{BufRead, Write};

    let mut shared = args.shared;
    let file_config = config::load_config(no_config)?;
    let mut history = load_session_history(shared.session.as_deref())?;
    let mut system_prompt = resolve_system_prompt(&shared, &file_config)?;
    let env = AppContext::new(&file_config);

    eprintln!("lait chat — /exit to quit, /clear to reset history, /model <name>, /system <text>");

    // Resolved lazily on first use rather than up front, so a `--model`-less
    // invocation still drops into the REPL instead of erroring immediately —
    // the user can `/model <name>` before ever sending a line. Cached across
    // turns after that (`resolve_chat_settings` does only cheap string/config
    // work, but nothing here changes turn to turn except in response to
    // `/model`, which invalidates it below).
    let mut settings: Option<RequestSettings> = None;

    let stdin = std::io::stdin();
    let repl = async {
        loop {
            eprint!("> ");
            std::io::stderr().flush()?;
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                break; // end-of-input (Ctrl-D)
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(command) = repl::parse_meta_command(line) {
                match command {
                    repl::MetaCommand::Exit => break,
                    repl::MetaCommand::Clear => {
                        history.clear();
                        eprintln!("(history cleared — a --session log, if any, is unaffected)");
                    }
                    repl::MetaCommand::Model(name) if !name.is_empty() => {
                        shared.model = Some(name.to_owned());
                        settings = None;
                        eprintln!("(model set to '{name}')");
                    }
                    repl::MetaCommand::Model(_) => eprintln!("usage: /model <name>"),
                    repl::MetaCommand::System(text) if !text.is_empty() => {
                        system_prompt = Some(text.to_owned());
                        eprintln!("(system prompt updated)");
                    }
                    repl::MetaCommand::System(_) => eprintln!("usage: /system <text>"),
                    repl::MetaCommand::Unknown(name) => eprintln!("unknown command: /{name}"),
                }
                continue;
            }

            if settings.is_none() {
                settings = match resolve_chat_settings(&shared, None, &file_config) {
                    Ok(resolved) => Some(resolved),
                    Err(error) => {
                        eprintln!("lait: {error:#}");
                        continue;
                    }
                };
            }
            let settings = settings
                .as_ref()
                .expect("just resolved above, or the loop continued before reaching here");

            match run_repl_turn(
                settings,
                &env,
                &system_prompt,
                &history,
                line,
                shared.show_reasoning,
                shared.show_usage,
            )
            .await
            {
                Ok((assistant_text, turn_usage)) => {
                    history.push(llm::user_message(line, &[])?);
                    history.push(llm::assistant_message(&assistant_text)?);
                    finish_chat_turn(
                        shared.session.as_deref(),
                        shared.no_history,
                        &file_config,
                        &settings.resolved_model.model_id,
                        line,
                        &assistant_text,
                        turn_usage,
                    )?;
                }
                // One bad turn (a request error, a bad `/model` name that only
                // fails once actually resolved) shouldn't end the whole session
                // — report it and let the user try again or `/exit`.
                Err(error) => eprintln!("lait: {error:#}"),
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    env.finish(repl).await
}

/// Runs one `lait chat` turn: streams the response to stdout (the REPL's
/// default), or — when `settings.mcp`/`settings.subagents` names at least
/// one tool source — falls back to a single non-streamed request printed
/// once it completes, since `RequestSettings::complete_stream` cannot yet
/// drive a tool loop (see its own doc comment). Returns the assistant's raw
/// reply text (never the `Reasoning:`-prefixed display form, the shape
/// `history`/a `--session` log need) alongside this turn's own token usage
/// (not `env.usage`'s running session total — see the `before`/`after`
/// delta below — since `env` persists across every REPL turn and `lait
/// history` wants each entry's own usage, not the cumulative session total).
async fn run_repl_turn(
    settings: &RequestSettings,
    env: &AppContext<'_>,
    system_prompt: &Option<String>,
    history: &[ChatCompletionRequestMessage],
    prompt: &str,
    show_reasoning: bool,
    show_usage: bool,
) -> Result<(String, Option<response::Usage>)> {
    let before = env.usage.total().unwrap_or_default();
    let turn = PromptTurn {
        system_prompt: system_prompt.as_deref(),
        history,
        prompt,
        image_urls: &[],
    };
    let content = if settings.mcp.is_empty() && settings.subagents.is_empty() {
        let stream = settings
            .complete_stream(&env.skill_cache, turn, None, show_usage)
            .await?;
        let outcome = stream_response(stream, show_reasoning, None).await?;
        if show_usage && let Some(usage) = outcome.usage {
            env.usage.record(&settings.usage_label, usage);
        }
        if show_usage {
            usage::print_usage_summary(&env.usage);
        }
        outcome.content
    } else {
        let response = settings
            .complete(env, &[], turn, None, env.cancel.clone())
            .await?;
        let rendered = response::render_response(&response, false, show_reasoning)?;
        println!("{rendered}");
        if show_usage {
            usage::print_usage_summary(&env.usage);
        }
        response::content_text(&response).to_owned()
    };
    let turn_usage = env.usage.total().map(|after| response::Usage {
        prompt_tokens: after.prompt_tokens.saturating_sub(before.prompt_tokens),
        completion_tokens: after
            .completion_tokens
            .saturating_sub(before.completion_tokens),
        total_tokens: after.total_tokens.saturating_sub(before.total_tokens),
    });
    Ok((content, turn_usage))
}

/// Runs `lait prompt <NAME> [INPUT]` (`args.name == "list"` is handled
/// separately, synchronously, by `prompt::list` — see `needs_async_runtime`/
/// `run_blocking`): renders the named prompt (see `prompt::render_named`)
/// and sends the result as a plain, tool-free, non-streamed request. This
/// subcommand form is intentionally narrower than `-p`/`--prompt-name` on
/// the main chat invocation (no `--model`/`--stream`/`-o`/... overrides —
/// see `docs/usage/ja/prompts.md`); reach for `-p` when those are needed.
async fn run_prompt(args: PromptArgs, no_config: bool) -> Result<()> {
    let file_config = config::load_config(no_config)?;
    if args.name == "list" {
        return prompt::list(&file_config);
    }

    let raw_input = resolve_input_with_stdin(args.input.clone())?
        .ok_or_else(|| anyhow!("an INPUT is required; provide one or pipe input via stdin"))?;
    let (prompt_text, prompt_model) =
        prompt::render_named(&args.name, &raw_input, &args.var, &file_config)?;

    let model_name = prompt_model
        .or_else(|| file_config.default.model.clone())
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "model is required for prompt '{}'; set 'prompts.{}.model' or default.model in {}",
                args.name,
                args.name,
                config::CONFIG_FILE_NAME
            )
        })?;
    let settings = resolve_request_settings(
        model_name,
        SamplingOverrides::default(),
        None,
        None,
        CapabilityOverrides::default(),
        &ModelMap::default(),
        &file_config,
    )?
    .with_usage_label(format!("prompt '{}'", args.name));

    let env = AppContext::new(&file_config);
    let response = env
        .finish(settings.complete(
            &env,
            &[],
            PromptTurn::simple(None, &prompt_text),
            None,
            env.cancel.clone(),
        ))
        .await?;
    let output = response::render_response(&response, false, false)?;
    println!("{output}");
    record_history(
        args.no_history,
        &file_config,
        "prompt",
        Some(&settings.resolved_model.model_id),
        &prompt_text,
        &output,
        env.usage.total(),
    )?;
    if args.show_usage {
        usage::print_usage_summary(&env.usage);
    }
    Ok(())
}

/// Prints the `<prefix> name: description` announcement line shared by
/// `run_agent`/`run_workflow` (prefix `==>`) and `execute_step`'s `workflow:`
/// branch (a progress-indented `->`): nothing when `name` is unset, and no
/// trailing `:` when `description` is.
fn announce_named_file(prefix: &str, name: Option<&str>, description: Option<&str>) {
    let Some(name) = name else { return };
    match description {
        Some(description) => eprintln!("{prefix} {name}: {description}"),
        None => eprintln!("{prefix} {name}"),
    }
}

async fn run_agent(args: AgentRunArgs, no_config: bool) -> Result<()> {
    let raw_input = resolve_input_with_stdin(args.input.clone())?
        .ok_or_else(|| anyhow!("an INPUT is required; provide one or pipe input via stdin"))?;
    let agent_file = agent::load_agent(&args.file)?;
    let canonical_agent_path = std::fs::canonicalize(&args.file).with_context(|| {
        format!(
            "failed to resolve agent file path '{}'",
            args.file.display()
        )
    })?;
    let file_config = config::load_config(no_config)?;

    announce_named_file(
        "==>",
        agent_file.name.as_deref(),
        agent_file.description.as_deref(),
    );

    let input = template::parse_input(&raw_input);
    agent_file
        .validate_input(&input)
        .with_context(|| format!("agent '{}'", args.file.display()))?;

    let usage_label = agent_file
        .name
        .clone()
        .unwrap_or_else(|| args.file.display().to_string());
    let settings =
        agent_file_settings(&agent_file, &file_config, None)?.with_usage_label(usage_label);

    let env = AppContext::new(&file_config);
    let output = env
        .finish(call_agent(
            &agent_file,
            &settings,
            &env,
            AgentTurn::simple(&input, &raw_input),
            &workflow::StepOutputs::new(),
            std::slice::from_ref(&canonical_agent_path),
            env.cancel.clone(),
        ))
        .await
        .with_context(|| format!("agent '{}'", args.file.display()))?;
    println!("{output}");
    record_history(
        args.no_history,
        &file_config,
        "agent",
        Some(&settings.resolved_model.model_id),
        &raw_input,
        &output,
        env.usage.total(),
    )?;
    if args.show_usage {
        usage::print_usage_summary(&env.usage);
    }
    Ok(())
}

async fn run_workflow(run_args: RunArgs, no_config: bool) -> Result<()> {
    let prompt = resolve_input_with_stdin(run_args.prompt.clone())?
        .ok_or_else(|| anyhow!("a PROMPT is required; provide one or pipe input via stdin"))?;
    let mut wf = workflow::load_workflow(&run_args.file)?;
    let file_config = config::load_config(no_config)?;

    announce_named_file("==>", wf.name.as_deref(), wf.description.as_deref());

    let scope = WorkflowScope::top_level(&mut wf, &run_args.file)?;
    let env = AppContext::new(&file_config);
    let initial_prompt = prompt.clone();
    let StepsOutcome {
        output: current_input,
        ..
    } = env
        .finish(run_steps(
            &wf.steps,
            prompt,
            workflow::StepOutputs::new(),
            RunStepsFrame {
                scope: &scope,
                env: &env,
                start_counter: 0,
                progress_prefix: "",
                cancellation: env.cancel.clone(),
            },
        ))
        .await?;
    println!("{current_input}");
    // A workflow can touch several models across its steps, so no single
    // `model` is recorded here — see `history::HistoryEntry::model`.
    record_history(
        run_args.no_history,
        &file_config,
        "workflow",
        None,
        &initial_prompt,
        &current_input,
        env.usage.total(),
    )?;
    if run_args.show_usage {
        usage::print_usage_summary(&env.usage);
    }
    Ok(())
}

/// The maximum `workflow:` nesting depth (a workflow step calling another
/// workflow file, whose own steps may call another, ...), rejected as a
/// runtime error rather than left to overflow the stack or hang.
pub(crate) const MAX_WORKFLOW_DEPTH: usize = 32;

/// Why entering a self-referential file (a `workflow:` node or a subagent)
/// failed `check_nesting_depth` below.
pub(crate) enum NestingDepthError {
    /// The file is already on the call stack.
    Cycle,
    /// Entering it would exceed the caller's `max_depth`.
    TooDeep,
}

/// Whether entering `canonical` from `active` (every file of the same kind
/// currently on the call stack, canonicalized) would create a cycle or
/// exceed `max_depth`. The generic core behind `check_workflow_nesting`
/// (`workflow:` nodes, `max_depth` = `MAX_WORKFLOW_DEPTH`) and
/// `call_subagent_tool` (`subagents:` calls, `max_depth` =
/// `MAX_SUBAGENT_DEPTH`), so both kinds of self-referential file nesting
/// share one cycle/depth-limit check instead of two copies of the same two
/// comparisons.
fn check_nesting_depth(
    active: &[PathBuf],
    canonical: &Path,
    max_depth: usize,
) -> Result<(), NestingDepthError> {
    if active.iter().any(|path| path == canonical) {
        return Err(NestingDepthError::Cycle);
    }
    if active.len() >= max_depth {
        return Err(NestingDepthError::TooDeep);
    }
    Ok(())
}

/// Whether entering `canonical` from `active` (every `workflow:` file
/// currently on the call stack, canonicalized) would create a cycle or
/// exceed `MAX_WORKFLOW_DEPTH`. Shared by `WorkflowScope::nested` (fails the
/// whole run) and `lint::lint_sub_workflow` (reports it as one more issue and
/// keeps linting the rest of the file), so the two can't drift on what counts
/// as too deep or cyclic.
pub(crate) fn check_workflow_nesting(
    active: &[PathBuf],
    canonical: &Path,
) -> Result<(), NestingDepthError> {
    check_nesting_depth(active, canonical, MAX_WORKFLOW_DEPTH)
}

/// The loaded config file, the MCP registry, the skill cache, the subagent
/// registry, and the run's top-level cancellation source for the whole
/// `lait`/`lait agent run`/`lait run` invocation — unlike `WorkflowScope`,
/// none of these change at a `workflow:` nesting boundary, so the same
/// `&AppContext` flows unchanged through every
/// `run_steps`/`execute_step_with_retry`/`execute_step` call (and, for
/// `call_agent`/`RequestSettings::complete`, through a subagent call's own
/// recursive completion too — see `call_subagent_tool`). Bundled into one
/// struct (rather than five parameters) purely to keep those functions'
/// argument counts under clippy's `too_many_arguments` threshold.
struct AppContext<'a> {
    file_config: &'a ConfigFile,
    registry: mcp::McpRegistry<'a>,
    skill_cache: skill::SkillCache<'a>,
    agent_registry: subagent::AgentRegistry<'a>,
    /// Every completion request's server-reported token usage, recorded by
    /// `RequestSettings::complete` and summarized when `--show-usage` asks
    /// for it.
    usage: usage::UsageTally,
    /// This invocation's own cancellation source, if any — the value every
    /// top-level `run_steps`/`complete` call seeds its own cancellation
    /// chain from (a node's own `timeout`/nested `workflow:` call then
    /// derives further child tokens off of that seed, see
    /// `execute_step_with_retry`). Currently always `None`: no caller wires
    /// up a real source (e.g. Ctrl-C) yet, but giving it one field here
    /// means a future one only has to change `new`'s caller, not every
    /// `run_steps`/`complete` call site.
    cancel: Option<tokio_util::sync::CancellationToken>,
}

impl<'a> AppContext<'a> {
    /// Builds the registries/cache over `file_config`'s named entries. Cheap:
    /// each one only borrows its config map — MCP connections, skill files,
    /// and agent files are all loaded lazily on first use.
    fn new(file_config: &'a ConfigFile) -> Self {
        Self {
            file_config,
            registry: mcp::McpRegistry::new(&file_config.mcp_servers),
            skill_cache: skill::SkillCache::new(&file_config.skills),
            agent_registry: subagent::AgentRegistry::new(&file_config.agents),
            usage: usage::UsageTally::default(),
            cancel: None,
        }
    }

    /// Drives `fut` to completion, then unconditionally shuts down the MCP
    /// registry before handing back `fut`'s result — on success or failure
    /// alike, so callers don't have to re-derive that ordering themselves.
    /// Every top-level `lait`/`lait agent run`/`lait run` invocation must
    /// call this once, at the end, instead of awaiting its work directly.
    async fn finish<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        let result = fut.await;
        self.registry.shutdown().await;
        result
    }
}

/// The default model/reasoning-effort, model aliases, and JSON schema
/// definitions currently in effect, plus enough bookkeeping to run a nested
/// `workflow:` step safely. Every `resolve_step_settings`/`execute_step` call
/// reads through this instead of a `&workflow::WorkflowFile` directly, so a
/// `workflow:` step's sub-workflow can see its own `default:`/`models:`/
/// `json_schemas:` first, falling back to its caller's (`nested` builds
/// that merge). `active_paths` records every workflow file currently
/// executing (canonicalized), to reject a `workflow:` cycle and to cap
/// nesting depth at `MAX_WORKFLOW_DEPTH`.
struct WorkflowScope {
    /// The `default:` block in effect for this scope's steps. Merged across
    /// `workflow:` nesting field by field — a sub-workflow's own entry wins,
    /// falling back to its caller's when unset (see
    /// `workflow::WorkflowDefaults::or_fallback`); only `retry` falls back as
    /// a whole struct rather than field-by-field.
    defaults: workflow::WorkflowDefaults,
    models: ModelMap,
    json_schemas: schema::JsonSchemaMap,
    /// This scope's own `nodes:` map, resolved by every `steps[].use` in this
    /// file. Unlike `models`/`json_schemas`, a `workflow:` node's sub-scope
    /// does *not* fall back to this scope's `nodes` for entries it lacks —
    /// each workflow file's `use:` sites only ever see that file's own
    /// `nodes:` (see `WorkflowScope::nested`).
    nodes: workflow::NodeMap,
    /// Directory relative paths in this scope's workflow file (currently
    /// only `node.workflow`) are resolved against.
    base_dir: PathBuf,
    active_paths: Vec<PathBuf>,
}

impl WorkflowScope {
    /// The scope for the workflow file passed on the command line. Takes
    /// `wf.default`/`wf.nodes` by move (via `mem::take`) rather than cloning
    /// them: neither is ever read again after this call, only `wf.steps`
    /// (see `run_workflow`).
    fn top_level(wf: &mut workflow::WorkflowFile, file_path: &Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(file_path).with_context(|| {
            format!(
                "failed to resolve workflow file path '{}'",
                file_path.display()
            )
        })?;
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            defaults: std::mem::take(&mut wf.default),
            models: wf.models.clone(),
            json_schemas: wf.json_schemas.clone(),
            nodes: std::mem::take(&mut wf.nodes),
            base_dir,
            active_paths: vec![canonical],
        })
    }

    /// The scope for a `workflow:` node's sub-workflow: resolves
    /// `relative_path` (as given in the node) against this scope's
    /// `base_dir`, merges `sub_wf`'s `default`/`models`/`json_schemas` over
    /// this scope's (the sub-workflow's own entries win; an entry it doesn't
    /// define falls back to this scope's), takes `sub_wf`'s `default`/`nodes:`
    /// by move (`nodes` gets no fallback — see `WorkflowScope::nodes` — and
    /// neither is ever read again after this call, only `sub_wf.steps`), and
    /// extends the cycle/depth bookkeeping. Fails if `relative_path`
    /// resolves to a workflow file already executing (a cycle) or nesting
    /// has reached `MAX_WORKFLOW_DEPTH`.
    fn nested(
        &self,
        relative_path: &Path,
        sub_wf: &mut workflow::WorkflowFile,
        label: &str,
    ) -> Result<Self> {
        let resolved_path = self.base_dir.join(relative_path);
        let canonical = std::fs::canonicalize(&resolved_path).with_context(|| {
            format!(
                "step '{label}': failed to resolve workflow file path '{}'",
                resolved_path.display()
            )
        })?;
        if let Err(error) = check_workflow_nesting(&self.active_paths, &canonical) {
            match error {
                NestingDepthError::Cycle => bail!(
                    "step '{label}': 'workflow: {}' would create a cycle ('{}' is already running)",
                    relative_path.display(),
                    canonical.display()
                ),
                NestingDepthError::TooDeep => bail!(
                    "step '{label}': 'workflow:' nesting exceeded the maximum depth of {MAX_WORKFLOW_DEPTH}"
                ),
            }
        }

        let mut models = sub_wf.models.clone();
        for (name, definitions) in &self.models {
            models
                .entry(name.clone())
                .or_insert_with(|| definitions.clone());
        }
        let mut json_schemas = sub_wf.json_schemas.clone();
        for (name, entry) in &self.json_schemas {
            json_schemas
                .entry(name.clone())
                .or_insert_with(|| entry.clone());
        }
        let mut active_paths = self.active_paths.clone();
        active_paths.push(canonical.clone());
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(Self {
            defaults: std::mem::take(&mut sub_wf.default).or_fallback(&self.defaults),
            models,
            json_schemas,
            nodes: std::mem::take(&mut sub_wf.nodes),
            base_dir,
            active_paths,
        })
    }
}

/// Sets `steps_outputs[step.label()]` to `output` (JSON-parsed, like a
/// `parallel` branch's output before joining). `FlowStep::label` is the
/// site's explicit `id`, else the node id it `use`s, else `None` for a
/// router site with no `id` — that case keeps the auto-generated `step-N`
/// progress label out of `{{ steps.* }}`/`$steps`, since that label isn't a
/// stable name to reference.
fn record_step_output(
    steps_outputs: &mut workflow::StepOutputs,
    step: &workflow::FlowStep,
    output: &str,
) {
    if let Some(key) = step.label() {
        steps_outputs.insert(key.to_string(), template::parse_input(output));
    }
}

/// A signal returned by `run_steps`, alongside its final input and progress
/// counter, describing how the run ended: `Continue` is the normal
/// end-of-list case; `Break`/`Stop` come from a `break: true`/`stop: true`
/// step (see `workflow::FlowStep`) and bubble up through `switch`/
/// `loop`/`for_each` frames until something catches them (`loop`/`for_each`
/// catch `Break`; nothing but `run_workflow` catches `Stop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Break,
    Stop,
}

/// The final input, the running progress counter, the `Flow` signal the run
/// ended with, and the named step outputs recorded along the way, returned by
/// `run_steps`.
struct StepsOutcome {
    output: String,
    counter: usize,
    flow: Flow,
    steps_outputs: workflow::StepOutputs,
}

/// Returns an error as soon as the cancellation inherited from an enclosing
/// timed step/workflow is observed. Router frames use this check between
/// child operations as well as passing the receiver into jq itself, so a
/// cancellation cannot be lost merely because a router has no model node of
/// its own.
fn check_workflow_cancellation(
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    if cancellation.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        bail!("workflow execution was cancelled");
    }
    Ok(())
}

/// Where in the overall run a call to `run_steps` sits, as opposed to
/// `steps`/`current_input`/`steps_outputs` (passed to `run_steps` directly),
/// which are what to run and the data flowing through it. Unchanged across
/// most recursive calls — `switch`/`loop`/a sequential `for_each` item/
/// `on_error` all reuse the caller's own frame fields — while a `parallel`
/// branch or a concurrent `for_each` item builds itself a fresh one with
/// `start_counter: 0` and a branch-local `progress_prefix`, and a nested
/// `workflow:` node's own call (from `execute_step`) builds one with a new
/// `scope` (see `WorkflowScope::nested`) and `cancellation` set to the
/// node's own `step_cancel`.
struct RunStepsFrame<'a> {
    scope: &'a WorkflowScope,
    env: &'a AppContext<'a>,
    start_counter: usize,
    progress_prefix: &'a str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
}

/// Runs a sequence of steps (the workflow's top-level `steps`, the nested
/// `steps` of a `switch` case/`else`, or a `parallel` branch), returning the
/// final input and the running progress counter so nested calls keep
/// numbering `[n]` labels continuously across the whole executed path
/// (skipped steps still consume a number). `frame.progress_prefix` is
/// prepended to every progress line, so a `parallel` branch's interleaved
/// output stays attributable to its branch; it is threaded through unchanged
/// by `switch` (only one case ever runs, so its numbering stays continuous
/// with the parent) but reset to a fresh branch-local prefix and counter by
/// `parallel` (every branch runs concurrently, so a single shared counter
/// would not reflect real execution order). `steps_outputs` is threaded the
/// same way as `current_input`/`counter` for a `switch` case, `loop`
/// iteration, or `for_each` item (each sees every id recorded so far, and its
/// own recordings flow to whatever runs after it), but is only ever cloned
/// into a `parallel` branch, never merged back: concurrently running branches
/// recording into a shared namespace would race, and there is no well-defined
/// "the" value for an id set differently by two branches. Boxed because a
/// `switch`/`parallel` step recurses into this function from within an
/// `async` body, which Rust cannot size otherwise. `frame.cancellation` is
/// cloned into every nested frame and router jq operation, preserving the
/// timeout of the enclosing step/workflow across control-flow boundaries.
fn run_steps<'a>(
    steps: &'a [workflow::FlowStep],
    current_input: String,
    steps_outputs: workflow::StepOutputs,
    frame: RunStepsFrame<'a>,
) -> Pin<Box<dyn Future<Output = Result<StepsOutcome>> + 'a>> {
    let RunStepsFrame {
        scope,
        env,
        start_counter,
        progress_prefix,
        cancellation,
    } = frame;
    Box::pin(async move {
        let mut current_input = current_input;
        let mut counter = start_counter;
        let mut steps_outputs = steps_outputs;
        for step in steps {
            check_workflow_cancellation(cancellation.as_ref())?;
            counter += 1;
            let label = step.label_or(counter);

            // `validate::validate_steps` guarantees at most one of
            // `switch`/`parallel`/`loop`/`for_each` is set, so `router()`
            // (which just checks them in a fixed order) can't silently
            // prefer one over another here. Matched exhaustively (no `_`
            // arm) so a new router kind fails to compile here until handled.
            match step.router() {
                Some(workflow::Router::Switch(switch)) => {
                    eprintln!("{progress_prefix}[{counter}] {label}");

                    let mut matched = None;
                    for (case_index, case) in switch.cases.iter().enumerate() {
                        if workflow::eval_when_async(
                            &case.when,
                            &current_input,
                            &steps_outputs,
                            cancellation.clone(),
                        )
                        .await
                        .with_context(|| format!("step '{label}'"))?
                        {
                            let case_label = case
                                .id
                                .clone()
                                .unwrap_or_else(|| format!("case-{}", case_index + 1));
                            eprintln!("{progress_prefix}    -> case '{case_label}' matched");
                            matched = Some(
                                run_steps(
                                    &case.steps,
                                    current_input.clone(),
                                    steps_outputs.clone(),
                                    RunStepsFrame {
                                        scope,
                                        env,
                                        start_counter: counter,
                                        progress_prefix,
                                        cancellation: cancellation.clone(),
                                    },
                                )
                                .await?,
                            );
                            break;
                        }
                    }
                    let StepsOutcome {
                        output: result,
                        counter: new_counter,
                        flow,
                        steps_outputs: new_steps_outputs,
                    } = match matched {
                        Some(result) => result,
                        None => match &switch.else_steps {
                            Some(else_steps) => {
                                eprintln!(
                                    "{progress_prefix}    -> no case matched, running 'else'"
                                );
                                run_steps(
                                    else_steps,
                                    current_input.clone(),
                                    steps_outputs.clone(),
                                    RunStepsFrame {
                                        scope,
                                        env,
                                        start_counter: counter,
                                        progress_prefix,
                                        cancellation: cancellation.clone(),
                                    },
                                )
                                .await?
                            }
                            None => {
                                bail!(
                                    "step '{label}': no case matched and no 'else' branch is defined"
                                )
                            }
                        },
                    };
                    current_input = result;
                    counter = new_counter;
                    steps_outputs = new_steps_outputs;
                    record_step_output(&mut steps_outputs, step, &current_input);
                    if flow != Flow::Continue {
                        return Ok(StepsOutcome {
                            output: current_input,
                            counter,
                            flow,
                            steps_outputs,
                        });
                    }
                    continue;
                }

                Some(workflow::Router::Parallel(parallel)) => {
                    eprintln!("{progress_prefix}[{counter}] {label}");
                    eprintln!(
                        "{progress_prefix}    -> running {} branches concurrently",
                        parallel.branches.len()
                    );

                    let branch_labels: Vec<String> = parallel
                        .branches
                        .iter()
                        .enumerate()
                        .map(|(index, branch)| branch.label(index))
                        .collect();
                    let branch_prefixes: Vec<String> = branch_labels
                        .iter()
                        .map(|branch_label| format!("{progress_prefix}[{branch_label}] "))
                        .collect();
                    let branch_futures = parallel.branches.iter().zip(&branch_prefixes).map(
                        |(branch, branch_prefix)| {
                            run_steps(
                                &branch.steps,
                                current_input.clone(),
                                steps_outputs.clone(),
                                RunStepsFrame {
                                    scope,
                                    env,
                                    start_counter: 0,
                                    progress_prefix: branch_prefix,
                                    cancellation: cancellation.clone(),
                                },
                            )
                        },
                    );
                    let branch_results = futures_util::future::try_join_all(branch_futures).await?;

                    // `validate_steps` rejects `stop`/`break` anywhere inside a
                    // `parallel` branch, so every branch always finishes with
                    // `Flow::Continue`; only its output is used here. Each branch
                    // got its own clone of `steps_outputs` (see this function's
                    // doc comment), so whatever it recorded stays branch-local.
                    let mut joined = serde_json::Map::new();
                    for (branch_label, branch_result) in
                        branch_labels.into_iter().zip(branch_results)
                    {
                        joined.insert(branch_label, template::parse_input(&branch_result.output));
                    }
                    let joined_json = serde_json::to_string(&serde_json::Value::Object(joined))
                        .context("failed to serialize joined 'parallel' branch outputs")?;

                    eprintln!("{progress_prefix}    -> branches joined");

                    current_input = match &parallel.join {
                        Some(filter) => jq::apply_cancellable_async(
                            filter,
                            &joined_json,
                            &steps_outputs,
                            cancellation.clone(),
                        )
                        .await
                        .with_context(|| format!("step '{label}'"))?,
                        None => joined_json,
                    };
                    record_step_output(&mut steps_outputs, step, &current_input);
                    continue;
                }

                Some(workflow::Router::Loop(loop_def)) => {
                    eprintln!("{progress_prefix}[{counter}] {label}");
                    // Validated by `validate::validate_steps`: exactly one of
                    // `while`/`until` is set, and `max_iterations` is `Some(n)` with n >= 1.
                    let max_iterations = loop_def
                        .max_iterations
                        .expect("loop.max_iterations is required by validate_steps");

                    let mut iteration_input = current_input.clone();
                    // Threaded continuously across iterations (like `switch`, unlike
                    // `parallel`'s per-branch reset): the loop body genuinely runs
                    // sequentially, so a single growing counter reflects real execution
                    // order.
                    let mut loop_counter = counter;
                    let mut iterations_run = 0usize;
                    // One driver for both condition kinds (`validate_steps`
                    // guarantees exactly one is set): `while` is checked before
                    // each iteration (so the body may run zero times), `until`
                    // after each one (so it always runs at least once). An
                    // explicit `break: true` ends the loop like a satisfied
                    // condition; exhausting `max_iterations` instead breaks
                    // with `satisfied` = false, an error either way.
                    let satisfied = loop {
                        if let Some(while_cond) = &loop_def.r#while
                            && !workflow::eval_when_async(
                                while_cond,
                                &iteration_input,
                                &steps_outputs,
                                cancellation.clone(),
                            )
                            .await
                            .with_context(|| format!("step '{label}'"))?
                        {
                            break true;
                        }
                        if iterations_run >= max_iterations {
                            break false;
                        }
                        iterations_run += 1;
                        eprintln!(
                            "{progress_prefix}    -> iteration {iterations_run}/{max_iterations}"
                        );
                        let StepsOutcome {
                            output: result,
                            counter: new_counter,
                            flow,
                            steps_outputs: new_steps_outputs,
                        } = run_steps(
                            &loop_def.steps,
                            iteration_input.clone(),
                            steps_outputs.clone(),
                            RunStepsFrame {
                                scope,
                                env,
                                start_counter: loop_counter,
                                progress_prefix,
                                cancellation: cancellation.clone(),
                            },
                        )
                        .await?;
                        iteration_input = result;
                        loop_counter = new_counter;
                        steps_outputs = new_steps_outputs;
                        match flow {
                            Flow::Continue => {}
                            Flow::Break => break true,
                            Flow::Stop => {
                                return Ok(StepsOutcome {
                                    output: iteration_input,
                                    counter: loop_counter,
                                    flow: Flow::Stop,
                                    steps_outputs,
                                });
                            }
                        }
                        if let Some(until_cond) = &loop_def.until
                            && workflow::eval_when_async(
                                until_cond,
                                &iteration_input,
                                &steps_outputs,
                                cancellation.clone(),
                            )
                            .await
                            .with_context(|| format!("step '{label}'"))?
                        {
                            break true;
                        }
                    };
                    if !satisfied {
                        let condition = if loop_def.r#while.is_some() {
                            "while"
                        } else {
                            "until"
                        };
                        bail!(
                            "step '{label}': 'loop' reached max_iterations ({max_iterations}) without satisfying '{condition}'"
                        );
                    }
                    current_input = iteration_input;
                    counter = loop_counter;
                    record_step_output(&mut steps_outputs, step, &current_input);
                    continue;
                }

                Some(workflow::Router::ForEach(for_each)) => {
                    eprintln!("{progress_prefix}[{counter}] {label}");
                    let items_json = jq::apply_one_cancellable_async(
                        &for_each.items,
                        &current_input,
                        &steps_outputs,
                        cancellation.clone(),
                    )
                    .await
                    .with_context(|| format!("step '{label}'"))?;
                    let items_value: serde_json::Value = serde_json::from_str(&items_json)
                        .with_context(|| {
                            format!(
                                "step '{label}': failed to parse 'for_each.items' output as JSON"
                            )
                        })?;
                    let items = items_value.as_array().cloned().ok_or_else(|| {
                        anyhow!("step '{label}': 'for_each.items' must produce a JSON array")
                    })?;

                    let max_concurrency = for_each.max_concurrency.unwrap_or(1);
                    let results: Vec<serde_json::Value> = if max_concurrency <= 1 {
                        eprintln!(
                            "{progress_prefix}    -> iterating over {} item(s)",
                            items.len()
                        );
                        let mut results = Vec::with_capacity(items.len());
                        // Threaded continuously across items, like `loop` (see its
                        // comment above): a sequential `for_each` (the default)
                        // runs its body one item at a time, so a single growing
                        // counter matches real execution order.
                        let mut for_each_counter = counter;
                        let mut stop_result = None;
                        for (item_index, item) in items.iter().enumerate() {
                            eprintln!(
                                "{progress_prefix}    -> item {}/{}",
                                item_index + 1,
                                items.len()
                            );
                            // A string item is passed through raw (like `parallel`'s
                            // `current_input`, and the inverse of `template::parse_input`
                            // used below for results), not re-quoted as JSON, so
                            // `{{ input }}` sees the same unquoted text everywhere else
                            // in the pipeline does.
                            let item_input =
                                value_to_input_text(item, "failed to serialize a 'for_each' item")?;
                            let StepsOutcome {
                                output: result,
                                counter: new_counter,
                                flow,
                                steps_outputs: new_steps_outputs,
                            } = run_steps(
                                &for_each.steps,
                                item_input,
                                steps_outputs.clone(),
                                RunStepsFrame {
                                    scope,
                                    env,
                                    start_counter: for_each_counter,
                                    progress_prefix,
                                    cancellation: cancellation.clone(),
                                },
                            )
                            .await?;
                            for_each_counter = new_counter;
                            steps_outputs = new_steps_outputs;
                            if flow == Flow::Stop {
                                stop_result = Some(result);
                                break;
                            }
                            results.push(template::parse_input(&result));
                            if flow == Flow::Break {
                                break;
                            }
                        }
                        counter = for_each_counter;
                        if let Some(result) = stop_result {
                            return Ok(StepsOutcome {
                                output: result,
                                counter,
                                flow: Flow::Stop,
                                steps_outputs,
                            });
                        }
                        results
                    } else {
                        eprintln!(
                            "{progress_prefix}    -> iterating over {} item(s), up to {max_concurrency} concurrently",
                            items.len()
                        );
                        let item_inputs: Vec<String> = items
                            .iter()
                            .map(|item| {
                                value_to_input_text(item, "failed to serialize a 'for_each' item")
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let item_prefixes: Vec<String> = (0..item_inputs.len())
                            .map(|index| format!("{progress_prefix}[item-{}] ", index + 1))
                            .collect();
                        let item_futures = item_inputs.into_iter().zip(&item_prefixes).map(
                            |(item_input, item_prefix)| {
                                run_steps(
                                    &for_each.steps,
                                    item_input,
                                    steps_outputs.clone(),
                                    RunStepsFrame {
                                        scope,
                                        env,
                                        start_counter: 0,
                                        progress_prefix: item_prefix,
                                        cancellation: cancellation.clone(),
                                    },
                                )
                            },
                        );
                        // `validate_steps` rejects `stop`/`break` inside a
                        // `for_each` body whose `max_concurrency` is above 1, for
                        // the same reason as a `parallel` branch: concurrently
                        // running items can't share a single well-defined "break
                        // this loop"/"stop the workflow" target. Each item also
                        // got its own clone of `steps_outputs` (see this
                        // function's doc comment), so nothing it records leaks
                        // back here.
                        let item_results: Vec<StepsOutcome> =
                            futures_util::stream::iter(item_futures)
                                .buffered(max_concurrency)
                                .try_collect()
                                .await?;
                        item_results
                            .into_iter()
                            .map(|outcome| template::parse_input(&outcome.output))
                            .collect()
                    };

                    let results_json = serde_json::to_string(&serde_json::Value::Array(results))
                        .context("failed to serialize 'for_each' results")?;

                    current_input = match &for_each.join {
                        Some(filter) => jq::apply_cancellable_async(
                            filter,
                            &results_json,
                            &steps_outputs,
                            cancellation.clone(),
                        )
                        .await
                        .with_context(|| format!("step '{label}'"))?,
                        None => results_json,
                    };
                    record_step_output(&mut steps_outputs, step, &current_input);
                    continue;
                }

                None => {}
            }

            if let Some(when) = &step.when {
                let truthy = workflow::eval_when_async(
                    when,
                    &current_input,
                    &steps_outputs,
                    cancellation.clone(),
                )
                .await
                .with_context(|| format!("step '{label}'"))?;
                if !truthy {
                    eprintln!("{progress_prefix}[{counter}] {label} (skipped)");
                    continue;
                }
            }

            eprintln!("{progress_prefix}[{counter}] {label}");
            current_input = match &step.r#use {
                None => current_input,
                Some(node_id) => {
                    // `validate::validate_steps` guarantees every `use:` site
                    // resolves against `scope.nodes` before execution starts.
                    let node = scope
                        .nodes
                        .get(node_id)
                        .expect("validate_steps guarantees 'use' resolves in 'nodes'");
                    let attempt_result = execute_step_with_retry(
                        node,
                        &current_input,
                        StepContext {
                            scope,
                            env,
                            label: &label,
                            progress_prefix,
                            steps_outputs: &steps_outputs,
                            step_cancel: cancellation.clone(),
                        },
                    )
                    .await;
                    match attempt_result {
                        Ok(output) => output,
                        Err(error) => match &step.on_error {
                            Some(on_error) => {
                                eprintln!(
                                    "{progress_prefix}    -> step failed, running 'on_error': {error}"
                                );
                                let error_input = serde_json::json!({
                                    "error": error.to_string(),
                                    "input": template::parse_input(&current_input),
                                });
                                let error_input_json = serde_json::to_string(&error_input)
                                    .context("failed to serialize 'on_error' input")?;
                                let StepsOutcome {
                                    output: result,
                                    counter: new_counter,
                                    flow,
                                    steps_outputs: new_steps_outputs,
                                } = run_steps(
                                    &on_error.steps,
                                    error_input_json,
                                    steps_outputs.clone(),
                                    RunStepsFrame {
                                        scope,
                                        env,
                                        start_counter: counter,
                                        progress_prefix,
                                        cancellation: cancellation.clone(),
                                    },
                                )
                                .await?;
                                counter = new_counter;
                                steps_outputs = new_steps_outputs;
                                if flow != Flow::Continue {
                                    // The handler's Break/Stop still completes this
                                    // step with `result`. Record that outer site's
                                    // output before bubbling the control-flow signal,
                                    // just like the router branches above do.
                                    record_step_output(&mut steps_outputs, step, &result);
                                    return Ok(StepsOutcome {
                                        output: result,
                                        counter,
                                        flow,
                                        steps_outputs,
                                    });
                                }
                                result
                            }
                            None => return Err(error),
                        },
                    }
                }
            };

            record_step_output(&mut steps_outputs, step, &current_input);

            if step.r#break == Some(true) {
                return Ok(StepsOutcome {
                    output: current_input,
                    counter,
                    flow: Flow::Break,
                    steps_outputs,
                });
            }
            if step.stop == Some(true) {
                return Ok(StepsOutcome {
                    output: current_input,
                    counter,
                    flow: Flow::Stop,
                    steps_outputs,
                });
            }
        }
        Ok(StepsOutcome {
            output: current_input,
            counter,
            flow: Flow::Continue,
            steps_outputs,
        })
    })
}

/// The upper bound on a single wait between retry attempts (see
/// `execute_step_with_retry`): a `retry` whose `delay_seconds`/`backoff`
/// (validated non-negative and finite by `workflow::validate`, but free to
/// grow exponentially) would wait longer than this waits this long instead —
/// a bounded, predictable worst case rather than an arbitrarily long hang.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(3600);

/// Runs `execute_step`, applying an effective timeout to each attempt and
/// retrying per an effective `retry` on failure (a timed-out attempt counts
/// as a failure). "Effective" means the node's own `retry`/`timeout` if set,
/// else `scope`'s `defaults.retry`/`defaults.timeout` (see
/// `WorkflowScope::defaults`) — but only for a node that calls a model
/// (`prompt`/`system_prompt`/`agent`, see `NodeDefinition::calls_model`): a
/// `jq`-only or `workflow:` node never falls back to the workflow default
/// (a `workflow:` node's own `retry`/`timeout` are
/// rejected by `validate::validate_node` in favor of the sub-workflow's own
/// steps setting theirs, and applying the *caller's* default on top of that
/// would double up whatever the sub-workflow's own steps already inherit).
/// Returns the last attempt's error once the effective `max_attempts` (or 1,
/// with no effective `retry`) is exhausted; the caller decides whether to run
/// `on_error` or propagate it. `label` is the calling `use:` site's label
/// (not the node's own id), so error messages point at where in the flow the
/// failure happened.
async fn execute_step_with_retry(
    node: &workflow::NodeDefinition,
    current_input: &str,
    context: StepContext<'_, '_>,
) -> Result<String> {
    let StepContext {
        scope,
        env,
        label,
        progress_prefix,
        steps_outputs,
        step_cancel: workflow_cancel,
    } = context;

    let calls_model = node.calls_model();
    let effective_retry = node.retry.as_ref().or(calls_model
        .then_some(scope.defaults.retry.as_ref())
        .flatten());
    let effective_timeout = node
        .timeout
        .or(calls_model.then_some(scope.defaults.timeout).flatten());

    let max_attempts = effective_retry
        .and_then(|retry| retry.max_attempts)
        .unwrap_or(1);
    let backoff = effective_retry
        .and_then(|retry| retry.backoff)
        .unwrap_or(1.0);
    let mut delay = Duration::from_secs(
        effective_retry
            .and_then(|retry| retry.delay_seconds)
            .unwrap_or(0),
    )
    .min(MAX_RETRY_DELAY);

    let mut attempt = 0usize;
    loop {
        attempt += 1;
        check_workflow_cancellation(workflow_cancel.as_ref())?;
        let outcome = match effective_timeout {
            // Keep the timeout around the whole node action (including its
            // later jq/write_file work). A cancellation channel is passed to
            // every timed node, not just command nodes: jq and write_file
            // also run outside Tokio and must be told to stop waiting before
            // a retry or an on_error branch starts. The future stays borrowed
            // until cancellation cleanup finishes, avoiding a second attempt
            // racing the child-owning or file-writing future.
            Some(seconds) => {
                // A child token is cancelled both by this node's own timeout
                // (below) and by `workflow_cancel` being cancelled (a
                // `CancellationToken` property, not something forwarded by
                // hand) — `execute_step` only ever needs to watch this one
                // token either way.
                let node_cancel = match &workflow_cancel {
                    Some(parent) => parent.child_token(),
                    None => tokio_util::sync::CancellationToken::new(),
                };
                let execution = execute_step(
                    node,
                    current_input,
                    StepContext {
                        scope,
                        env,
                        label,
                        progress_prefix,
                        steps_outputs,
                        step_cancel: Some(node_cancel.clone()),
                    },
                );
                tokio::pin!(execution);
                match tokio::time::timeout(Duration::from_secs(seconds), &mut execution).await {
                    Ok(result) => result,
                    Err(_) => {
                        node_cancel.cancel();
                        let _ = execution.await;
                        Err(anyhow!(
                            "step '{label}' timed out after {seconds}s (attempt {attempt}/{max_attempts})"
                        ))
                    }
                }
            }
            None => {
                execute_step(
                    node,
                    current_input,
                    StepContext {
                        scope,
                        env,
                        label,
                        progress_prefix,
                        steps_outputs,
                        step_cancel: workflow_cancel.clone(),
                    },
                )
                .await
            }
        };

        match outcome {
            Ok(output) => return Ok(output),
            Err(error) if attempt < max_attempts => {
                check_workflow_cancellation(workflow_cancel.as_ref())?;
                eprintln!(
                    "{progress_prefix}    -> attempt {attempt}/{max_attempts} failed: {error}; retrying in {:.1}s",
                    delay.as_secs_f64()
                );
                wait_retry_delay(delay, workflow_cancel.as_ref()).await?;
                // `try_from_secs_f64` + the `MAX_RETRY_DELAY` clamp keep an
                // exponentially growing (or pathological) delay from
                // overflowing `Duration` — `Duration::from_secs_f64` would
                // panic there instead of just waiting the capped hour.
                delay = Duration::try_from_secs_f64((delay.as_secs_f64() * backoff).max(0.0))
                    .unwrap_or(MAX_RETRY_DELAY)
                    .min(MAX_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Sleeps between retries while still honoring cancellation inherited from a
/// surrounding workflow. A plain `sleep` would allow a cancelled nested
/// workflow to wait for an arbitrarily large backoff before returning.
async fn wait_retry_delay(
    delay: Duration,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    let Some(cancellation) = cancellation else {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        return Ok(());
    };
    if delay.is_zero() {
        if cancellation.is_cancelled() {
            bail!("workflow execution was cancelled");
        }
        return Ok(());
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => bail!("workflow execution was cancelled"),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

/// The state a single node execution needs, bundled so
/// `execute_step_with_retry`/`execute_step` take one parameter instead of
/// six. `step_cancel` is the cancellation in effect for this particular
/// attempt — the caller's own cancellation on the first attempt of a node
/// with no `timeout`, or a child token scoped to just that attempt when a
/// `timeout` is set (see `execute_step_with_retry`) — which is why it lives
/// here rather than on `AppContext`: it changes across attempts and nesting
/// depths, unlike everything on `AppContext`, which does not.
#[derive(Clone)]
struct StepContext<'a, 'env> {
    scope: &'a WorkflowScope,
    env: &'a AppContext<'env>,
    label: &'a str,
    progress_prefix: &'a str,
    steps_outputs: &'a workflow::StepOutputs,
    step_cancel: Option<tokio_util::sync::CancellationToken>,
}

/// Resolves the model/reasoning-effort settings for a node's model call,
/// applying the node > agent file (when this node has one) > workflow
/// default precedence chain shared by `execute_step`'s `agent` and `prompt`
/// branches. `agent_file` is `Some` only for an `agent` node; besides adding
/// its fallback layer, its presence also selects which hint text a
/// missing-model error uses.
fn resolve_step_settings(
    node: &workflow::NodeDefinition,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    agent_file: Option<&AgentFile>,
    label: &str,
) -> Result<RequestSettings> {
    let model_name = node
        .model
        .clone()
        .or_else(|| agent_file.and_then(|agent_file| agent_file.model.clone()))
        .or_else(|| scope.defaults.model.clone())
        .or_else(|| file_config.default.model.clone())
        .ok_or_else(|| {
            anyhow!(
                "model is required for step '{label}'; set it on the node,{} the workflow's default.model, or in {}",
                if agent_file.is_some() { " its agent file," } else { "" },
                config::CONFIG_FILE_NAME
            )
        })?;
    let overrides = SamplingOverrides {
        reasoning_effort: node
            .reasoning_effort
            .or(agent_file.and_then(|agent_file| agent_file.reasoning_effort))
            .or(scope.defaults.reasoning_effort),
        temperature: node
            .temperature
            .or(agent_file.and_then(|agent_file| agent_file.temperature))
            .or(scope.defaults.temperature),
        top_p: node
            .top_p
            .or(agent_file.and_then(|agent_file| agent_file.top_p))
            .or(scope.defaults.top_p),
        max_tokens: node
            .max_tokens
            .or(agent_file.and_then(|agent_file| agent_file.max_tokens))
            .or(scope.defaults.max_tokens),
    };
    let mcp = node
        .mcp
        .clone()
        .or_else(|| agent_file.and_then(|agent_file| agent_file.mcp.clone()))
        .or_else(|| scope.defaults.mcp.clone());
    let max_tool_rounds = node
        .max_tool_rounds
        .or_else(|| agent_file.and_then(|agent_file| agent_file.max_tool_rounds))
        .or(scope.defaults.max_tool_rounds);
    let skills = node
        .skills
        .clone()
        .or_else(|| agent_file.and_then(|agent_file| agent_file.skills.clone()))
        .or_else(|| scope.defaults.skills.clone());
    let subagents = node
        .subagents
        .clone()
        .or_else(|| agent_file.and_then(|agent_file| agent_file.subagents.clone()))
        .or_else(|| scope.defaults.subagents.clone());
    resolve_request_settings(
        model_name,
        overrides,
        None,
        None,
        CapabilityOverrides {
            mcp,
            max_tool_rounds,
            skills,
            subagents,
        },
        &scope.models,
        file_config,
    )
    .with_context(|| format!("step '{label}'"))
}

/// Resolves a node's `files:`/`images:` attachments against `base_prompt`:
/// file contents become a named fenced code block appended after it
/// (`base_prompt` unchanged when `files` is unset), and image paths/URLs
/// resolve into `image_url` content parts for the caller's eventual
/// `AgentTurn`/`PromptTurn`. The two kinds are read/resolved concurrently
/// since they're otherwise-independent I/O. Shared by `execute_step`'s
/// `agent` and `sends_prompt` branches, which each attach to a different
/// "base" user message (the current input passed through unchanged, vs. the
/// rendered `prompt` template).
async fn resolve_attachments<'a>(
    node: &workflow::NodeDefinition,
    base_prompt: &'a str,
    label: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<(Cow<'a, str>, Vec<String>)> {
    let (file_context, image_urls) = tokio::try_join!(
        attachment::read_file_attachments_cancellable(
            node.files.as_deref().unwrap_or(&[]),
            cancellation.clone(),
        ),
        attachment::resolve_image_urls_cancellable(
            node.images.as_deref().unwrap_or(&[]),
            cancellation,
        ),
    )
    .with_context(|| format!("step '{label}'"))?;
    let prompt = match file_context {
        Some(context) => Cow::Owned(format!("{base_prompt}\n\n{context}")),
        None => Cow::Borrowed(base_prompt),
    };
    Ok((prompt, image_urls))
}

/// Runs a single node (agent call, prompt call, or `jq`-only data transform)
/// and returns its output, with `jq` applied afterward if set. `label` is the
/// calling `use:` site's label, used only for progress output/error messages.
async fn execute_step(
    node: &workflow::NodeDefinition,
    current_input: &str,
    context: StepContext<'_, '_>,
) -> Result<String> {
    let StepContext {
        scope,
        env,
        label,
        progress_prefix,
        steps_outputs,
        step_cancel,
    } = context;
    if let Some(name_or_path) = &node.input_schema {
        let schema = schema::resolve_named_schema_value_cancellable(
            &scope.json_schemas,
            name_or_path,
            step_cancel.clone(),
        )
        .await
        .with_context(|| format!("step '{label}'"))?;
        let input = template::parse_input(current_input);
        schema::validate_input_against_schema(&schema, &input)
            .with_context(|| format!("step '{label}'"))?;
    }

    let mut step_output = if let Some(agent_path) = &node.agent {
        // Loaded through the registry's path cache (not `agent::load_agent`
        // directly) so a `for_each`/`loop` body re-running this node reuses
        // the parsed file and its resolved input schema instead of re-reading
        // both from disk on every iteration.
        let loaded = env
            .agent_registry
            .load_path_cancellable(agent_path, step_cancel.clone())
            .await
            .with_context(|| format!("step '{label}'"))?;
        let agent_file = &loaded.file;

        let input = template::parse_input(current_input);
        loaded
            .validate_input(&input)
            .with_context(|| format!("step '{label}'"))?;

        let settings =
            resolve_step_settings(node, scope, env.file_config, Some(agent_file), label)?
                .with_usage_label(label);

        let (prompt, image_urls) =
            resolve_attachments(node, current_input, label, step_cancel.clone()).await?;

        call_agent(
            agent_file,
            &settings,
            env,
            AgentTurn {
                input: &input,
                prompt: &prompt,
                image_urls: &image_urls,
            },
            steps_outputs,
            std::slice::from_ref(&loaded.canonical_path),
            step_cancel.clone(),
        )
        .await
        .with_context(|| format!("step '{label}'"))?
    } else if node.sends_prompt() {
        let settings = resolve_step_settings(node, scope, env.file_config, None, label)?
            .with_usage_label(label);

        let response_format = match node.output_schema.as_deref() {
            Some(name_or_path) => {
                let schema_name = node.schema_name.as_deref().unwrap_or("structured_output");
                let response_format = match scope.json_schemas.get(name_or_path) {
                    Some(entry) => {
                        schema::build_response_format_from_entry_cancellable(
                            entry,
                            schema_name,
                            step_cancel.clone(),
                        )
                        .await
                    }
                    None => {
                        schema::load_json_schema_cancellable(
                            Path::new(name_or_path),
                            schema_name,
                            step_cancel.clone(),
                        )
                        .await
                    }
                };
                Some(response_format.with_context(|| format!("step '{label}'"))?)
            }
            None => None,
        };

        let input = template::parse_input(current_input);
        // A `system_prompt`-only node (no `prompt`) sends the current input
        // unchanged as the user message, the same way an `agent` node's
        // `current_input` passes straight through `call_agent` without going
        // through `template::render`.
        let prompt: Cow<'_, str> = match &node.prompt {
            Some(prompt_template) => Cow::Owned(
                template::render(
                    prompt_template,
                    &input,
                    steps_outputs,
                    &serde_json::Map::new(),
                )
                .with_context(|| format!("step '{label}'"))?,
            ),
            None => Cow::Borrowed(current_input),
        };
        let (prompt, image_urls) =
            resolve_attachments(node, &prompt, label, step_cancel.clone()).await?;
        let system_prompt = node
            .system_prompt
            .as_deref()
            .or(scope.defaults.system_prompt.as_deref())
            .map(|system_prompt_template| {
                template::render(
                    system_prompt_template,
                    &input,
                    steps_outputs,
                    &serde_json::Map::new(),
                )
            })
            .transpose()
            .with_context(|| format!("step '{label}'"))?;

        let response = settings
            .complete(
                env,
                &[],
                PromptTurn {
                    system_prompt: system_prompt.as_deref(),
                    history: &[],
                    prompt: &prompt,
                    image_urls: &image_urls,
                },
                response_format,
                step_cancel.clone(),
            )
            .await
            .with_context(|| format!("step '{label}'"))?;

        response::render_response(&response, false, false)
            .with_context(|| format!("step '{label}'"))?
    } else if let Some(sub_workflow_path) = &node.workflow {
        let resolved_path = scope.base_dir.join(sub_workflow_path);
        let mut sub_wf =
            workflow::load_workflow(&resolved_path).with_context(|| format!("step '{label}'"))?;
        let sub_scope = scope.nested(sub_workflow_path, &mut sub_wf, label)?;
        announce_named_file(
            &format!("{progress_prefix}    ->"),
            sub_wf.name.as_deref(),
            sub_wf.description.as_deref(),
        );
        // Isolated like an `agent:` call, not threaded like a `switch` case:
        // the sub-workflow is a separate file with its own step ids, so it
        // starts with an empty `steps_outputs` and its Flow (whether it
        // ended via `stop`/`break` internally or just ran out of steps) is
        // this step's own concern, not the caller's — only its final output
        // crosses back.
        let sub_progress_prefix = format!("{progress_prefix}    ");
        let StepsOutcome { output: result, .. } = run_steps(
            &sub_wf.steps,
            current_input.to_string(),
            workflow::StepOutputs::new(),
            RunStepsFrame {
                scope: &sub_scope,
                env,
                start_counter: 0,
                progress_prefix: &sub_progress_prefix,
                cancellation: step_cancel.clone(),
            },
        )
        .await
        .with_context(|| format!("step '{label}'"))?;
        result
    } else if let Some(argv) = &node.command {
        let input = template::parse_input(current_input);
        let rendered_argv: Vec<String> = argv
            .iter()
            .map(|arg| template::render(arg, &input, steps_outputs, &serde_json::Map::new()))
            .collect::<Result<_>>()
            .with_context(|| format!("step '{label}'"))?;
        crate::process::run_command(&rendered_argv, current_input, step_cancel.clone())
            .await
            .with_context(|| format!("step '{label}'"))?
    } else {
        current_input.to_string()
    };

    if let Some(filter) = &node.jq {
        step_output = apply_jq(filter, &step_output, steps_outputs, step_cancel.as_ref())
            .await
            .with_context(|| format!("step '{label}'"))?;
    }

    if let Some(path) = &node.write_file {
        write_output_file(path, &step_output, step_cancel)
            .await
            .with_context(|| format!("step '{label}'"))?;
    }

    Ok(step_output)
}

/// Applies a node's jq transform off the Tokio workers. jq evaluation is
/// synchronous and can be expensive for a large input; running it on a
/// dedicated OS thread means the enclosing node timeout remains effective.
/// The worker receives a cooperative cancellation flag and is awaited after a
/// timeout, so a cancelled evaluation does not continue as a detached thread
/// after the workflow attempt has moved on.
async fn apply_jq(
    filter: &str,
    input: &str,
    steps_outputs: &workflow::StepOutputs,
    step_cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let cancellation = step_cancel.cloned();
    // Input normalization is deliberately performed inside the bounded jq
    // worker. A large plain-text model/command result must not be parsed and
    // re-serialized on a Tokio executor thread before cancellation can win.
    jq::apply_cancellable_async(filter, input, steps_outputs, cancellation).await
}

/// Writes a node's output from a dedicated OS thread. The worker is kept off
/// Tokio's runtime because a write to a special file such as a FIFO can block
/// indefinitely. A timeout sets the worker's cancellation flag and waits for
/// it to finish; Unix special files are opened non-blocking so that this
/// cleanup cannot itself get stuck. Regular files use the same direct
/// create/truncate/write behavior as `fs::write`, with cancellation checks
/// between bounded chunks so existing inode, permission, hard-link, and
/// symlink semantics remain intact.
async fn write_output_file(
    path: &Path,
    output: &str,
    step_cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<()> {
    let path = path.to_owned();
    let output = output.to_owned();
    // `run_blocking_with_path_lock` deliberately returns after a bounded
    // cancellation cleanup even when an OS/network filesystem call ignores
    // the cancellation flag; transferring the lease to the worker prevents a
    // retry from writing the same path concurrently with that still-running
    // worker.
    let worker_path = path.clone();
    async_io::run_blocking_with_path_lock(
        &path,
        move |cancelled| {
            write_output_file_blocking(&worker_path, &output, cancelled)
                .with_context(|| format!("failed to write output to '{}'", worker_path.display()))
        },
        step_cancel,
    )
    .await
}

/// Performs the blocking half of [`write_output_file`]. On Unix, the target is
/// opened once with `O_NONBLOCK` and classified from that same handle. This
/// removes the metadata-then-open TOCTOU window while preserving symlink,
/// inode, permission, and hard-link behavior for regular files. FIFOs and
/// other non-regular files continue through non-blocking I/O. Other platforms
/// reject non-regular handles after a conservative path preflight, rather than
/// attempting to write a device, named pipe, or reparse point.
fn write_output_file_blocking(path: &Path, output: &str, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("output file write was cancelled");
    }
    #[cfg(unix)]
    {
        use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};

        let file = loop {
            if cancelled.load(Ordering::Acquire) {
                bail!("output file write was cancelled");
            }
            match OpenOptions::new()
                .write(true)
                .create(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
            {
                Ok(file) => break file,
                // Opening a FIFO for writing without a reader reports ENXIO
                // when O_NONBLOCK is set. Poll until a reader appears or the
                // workflow cancellation flag asks us to stop.
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        };

        if !file.metadata()?.file_type().is_file() {
            return write_nonblocking_special_file(file, output, cancelled);
        }

        write_regular_output_file(file, output, cancelled)
    }

    #[cfg(not(unix))]
    {
        // Windows has no portable non-blocking File API. Reject an already
        // visible special/reparse target before opening it, then repeat the
        // check on the opened handle to keep a path swap from turning into a
        // write to a device or named pipe. Symlinks to regular files retain
        // the existing follow-and-overwrite behavior.
        match std::fs::metadata(path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                bail!(
                    "refusing to write non-regular output path '{}'",
                    path.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(path)?;
        if !file.metadata()?.file_type().is_file() {
            bail!(
                "refusing to write non-regular output path '{}'",
                path.display()
            );
        }
        write_regular_output_file(file, output, cancelled)
    }
}

/// Writes an ordinary file directly, preserving the target inode and the
/// overwrite/permission behavior of `fs::write`. Chunking only exists to give
/// a timed worker a bounded opportunity to observe cancellation.
fn write_regular_output_file(
    mut file: std::fs::File,
    output: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("output file write was cancelled");
    }
    // Truncate only after the handle has been classified as a regular file.
    // A timeout after this point intentionally leaves an empty/partial file:
    // direct truncation is what preserves the existing inode, permissions,
    // hard links, and symlink-following semantics of `fs::write`, but it is
    // not an atomic replacement. The caller receives an error and must not
    // treat the partial bytes as a completed node output.
    file.set_len(0)?;
    for chunk in output.as_bytes().chunks(64 * 1024) {
        if cancelled.load(Ordering::Acquire) {
            bail!("output file write was cancelled");
        }
        file.write_all(chunk)?;
    }
    file.flush()?;
    if cancelled.load(Ordering::Acquire) {
        bail!("output file write was cancelled");
    }
    Ok(())
}

#[cfg(unix)]
/// Writes FIFOs and other Unix special files with non-blocking I/O. Opening
/// the descriptor with `O_NONBLOCK` by the caller means no system
/// call can hold the worker past cancellation. The same handle is used for
/// classification and writing; reopening the path here would reintroduce a
/// metadata/open TOCTOU race.
fn write_nonblocking_special_file(
    mut file: std::fs::File,
    output: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    let bytes = output.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if cancelled.load(Ordering::Acquire) {
            bail!("output file write was cancelled");
        }
        match file.write(&bytes[offset..]) {
            Ok(0) => bail!("output file write made no progress"),
            Ok(written) => offset += written,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    if cancelled.load(Ordering::Acquire) {
        bail!("output file write was cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AppContext, RunStepsFrame, WorkflowScope, apply_jq, run_steps, write_output_file};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn a_cancelled_regular_write_does_not_truncate_an_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "lait-cancelled-output-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        std::fs::write(&path, "original").expect("failed to create output fixture");
        let token = CancellationToken::new();
        token.cancel();

        let result = write_output_file(&path, "replacement", Some(token)).await;
        let contents = std::fs::read_to_string(&path).expect("output fixture should remain");
        std::fs::remove_file(&path).expect("failed to remove output fixture");

        assert!(result.is_err(), "a cancelled write should fail");
        assert_eq!(contents, "original");
    }

    #[tokio::test]
    async fn an_already_cancelled_jq_stops_before_returning_a_value() {
        let token = CancellationToken::new();
        token.cancel();
        let steps = crate::workflow::StepOutputs::new();
        let started = std::time::Instant::now();

        // The filter is intentionally expensive if it is allowed to run. A
        // pre-set step cancellation must be observed immediately, before a
        // caller can mistake a value from the worker for a successful step.
        let result = apply_jq("range(0; 1000000000)", "null", &steps, Some(&token)).await;

        assert!(result.is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "pre-cancelled jq took too long to stop: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_router_condition_observes_the_enclosing_workflow_cancellation() {
        let path = crate::test_support::unique_temp_path("lait-router-cancel", ".yml");
        std::fs::write(
            &path,
            r#"
steps:
  - switch:
      cases:
        - when: 'reduce range(0; 100000000) as $i (false; .)'
          steps:
            - stop: true
      else:
        - stop: true
"#,
        )
        .expect("router workflow fixture should be writable");
        let mut workflow = crate::workflow::load_workflow(&path).unwrap();
        let scope = WorkflowScope::top_level(&mut workflow, &path).unwrap();
        let config = crate::config::ConfigFile::default();
        let env = AppContext::new(&config);
        let token = CancellationToken::new();
        let started = std::time::Instant::now();
        let execution = run_steps(
            &workflow.steps,
            "null".to_owned(),
            crate::workflow::StepOutputs::new(),
            RunStepsFrame {
                scope: &scope,
                env: &env,
                start_counter: 0,
                progress_prefix: "",
                cancellation: Some(token.clone()),
            },
        );
        tokio::pin!(execution);
        let result = tokio::select! {
            result = &mut execution => result,
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                token.cancel();
                tokio::time::timeout(std::time::Duration::from_secs(2), &mut execution)
                    .await
                    .expect("cancelled router should stop promptly")
            }
        };
        let _ = std::fs::remove_file(path);
        assert!(result.is_err(), "a cancelled router must not succeed");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "router cancellation took too long: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_fifo_write_is_joined_before_a_retry_can_write() {
        use std::{
            fs::OpenOptions,
            io::{ErrorKind, Read},
            os::unix::fs::OpenOptionsExt,
            sync::mpsc,
            time::Duration,
        };

        let path = crate::test_support::unique_temp_path("lait-retry-output-fifo", "");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo should be available on Unix");
        assert!(status.success());

        // Keep a reader open without consuming anything. The first writer
        // therefore fills the pipe and remains blocked, which makes a
        // detached writer observable when the retry starts.
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .expect("FIFO reader should open without a writer");
        let first_output = "x".repeat(512 * 1024);
        let retry_output = "y".repeat(512 * 1024);
        let cancel_token = CancellationToken::new();
        let first_path = path.clone();
        let first_output_for_task = first_output.clone();
        let first_token = cancel_token.clone();
        let first = tokio::spawn(async move {
            write_output_file(&first_path, &first_output_for_task, Some(first_token)).await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_token.cancel();
        let first_result = tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .expect("cancelling the first FIFO writer should finish promptly")
            .unwrap();
        assert!(first_result.is_err());

        // Start the retry while the reader is still paused. If the first
        // writer was not joined above, both writers will eventually publish
        // their complete payload into the same FIFO.
        let second_path = path.clone();
        let second_output = retry_output.clone();
        let second =
            tokio::spawn(
                async move { write_output_file(&second_path, &second_output, None).await },
            );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (read_done, read_result) = mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            let mut reader = reader;
            let mut received = Vec::new();
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => received.extend_from_slice(&buffer[..read]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("failed to read retry FIFO: {error}"),
                }
            }
            read_done.send(received).unwrap();
        });

        tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("the retry writer should finish after the reader drains the FIFO")
            .unwrap()
            .expect("the retry writer should succeed");
        reader_thread.join().unwrap();
        let received = read_result
            .recv_timeout(Duration::from_secs(1))
            .expect("FIFO reader should observe EOF after the retry writer closes");

        assert!(
            received.len() >= retry_output.len(),
            "retry FIFO received {} bytes, less than the retry payload of {}",
            received.len(),
            retry_output.len()
        );
        let first_prefix_len = received.len() - retry_output.len();
        assert!(
            first_prefix_len <= first_output.len(),
            "cancelled FIFO writer published {} bytes after the retry started",
            first_prefix_len.saturating_sub(first_output.len())
        );
        assert!(
            received[..first_prefix_len]
                .iter()
                .all(|byte| *byte == b'x'),
            "the cancelled writer's bytes must precede the retry payload"
        );
        assert_eq!(&received[first_prefix_len..], retry_output.as_bytes());
        std::fs::remove_file(path).unwrap();
    }
}
