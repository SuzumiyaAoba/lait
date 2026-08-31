use std::{
    borrow::Cow,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::ChatCompletionRequestMessage;
use futures_util::{StreamExt, TryStreamExt};

use crate::{
    agent::{self, AgentFile},
    async_io, attachment,
    cli::{AgentAction, ChatArgs, ChatReplArgs, Cli, Command, LintArgs, RunArgs},
    cli::{AgentRunArgs, PromptArgs, SharedChatArgs},
    config::{self, ConfigFile, ModelMap},
    docgen,
    engine::{
        AgentTurn, AppContext, CapabilityOverrides, PromptTurn, RequestSettings, SamplingOverrides,
        agent_file_settings, call_agent, resolve_request_settings, stream_response,
        value_to_input_text,
    },
    history, jq, lint, llm, nesting, prompt, render, repl, response, schema, session, template,
    usage, workflow,
};

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
        if let Err(error) = nesting::check_workflow_nesting(&self.active_paths, &canonical) {
            match error {
                nesting::NestingDepthError::Cycle => bail!(
                    "step '{label}': 'workflow: {}' would create a cycle ('{}' is already running)",
                    relative_path.display(),
                    canonical.display()
                ),
                nesting::NestingDepthError::TooDeep => bail!(
                    "step '{label}': 'workflow:' nesting exceeded the maximum depth of {}",
                    nesting::MAX_WORKFLOW_DEPTH
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
        async_io::write_output_file(path, &step_output, step_cancel)
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

#[cfg(test)]
mod tests {
    use super::{AppContext, RunStepsFrame, WorkflowScope, apply_jq, run_steps};
    use tokio_util::sync::CancellationToken;

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
}
