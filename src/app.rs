use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::ChatCompletionRequestMessage;

use crate::{
    agent, attachment,
    cli::{AgentAction, ChatArgs, ChatReplArgs, Cli, Command, PromptAction, RunArgs},
    cli::{AgentRunArgs, PromptRunArgs, SharedChatArgs},
    config::{self, ConfigFile, ConfigSource, ModelMap},
    docgen,
    engine::{
        AgentTurn, AppContext, CapabilityOverrides, PromptTurn, RequestSettings, SamplingOverrides,
        agent_file_settings, call_agent, resolve_request_settings, stream_response,
    },
    history, lint, prompt, repl, report, response, schema, session, template, usage,
    workflow::{
        self, WorkflowScope,
        exec::{RunStepsFrame, StepsOutcome, announce_named_file, run_steps},
    },
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

/// Records one finished chat turn: appends it to `--session`'s log (when
/// set) and to `lait history` (unless suppressed) — the shared tail of
/// `run_chat`'s streamed and non-streamed paths and `repl::run`'s per-turn
/// loop.
pub(crate) fn finish_chat_turn(
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
    report::record_history(
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
    let config_source = ConfigSource::from(&cli);
    match cli.command {
        Some(Command::Run(run_args)) => run_workflow(run_args, config_source).await,
        Some(Command::Agent(agent_command)) => match agent_command.action {
            AgentAction::Run(args) => run_agent(args, config_source).await,
        },
        Some(Command::Lint(lint_args)) => lint::run(lint_args, config_source),
        Some(Command::Models(models_args)) => crate::models::run(models_args, config_source).await,
        Some(Command::Completions(completions_args)) => {
            docgen::generate_completions(completions_args);
            Ok(())
        }
        Some(Command::Man(man_args)) => docgen::generate_man_pages(man_args),
        Some(Command::Init(init_args)) => crate::init::run(init_args),
        Some(Command::Sessions(sessions_command)) => crate::session::run(sessions_command),
        Some(Command::Chat(chat_repl_args)) => repl::run(chat_repl_args, config_source).await,
        Some(Command::Prompt(prompt_command)) => match prompt_command.action {
            PromptAction::List => {
                bail!("internal error: `prompt list` must run on the sync path")
            }
            PromptAction::Run(run_args) => run_prompt(run_args, config_source).await,
        },
        Some(Command::History(history_args)) => history::run(history_args),
        None => run_chat_or_repl(cli.chat, config_source).await,
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
async fn run_chat_or_repl(chat: ChatArgs, config_source: ConfigSource) -> Result<()> {
    use std::io::IsTerminal;

    match resolve_input_with_stdin(chat.prompt.clone())? {
        Some(prompt) => run_chat(chat, prompt, config_source).await,
        None if std::io::stdin().is_terminal() => {
            repl::run(
                ChatReplArgs {
                    shared: chat.shared,
                },
                config_source,
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
        Some(Command::Prompt(prompt_command)) => {
            matches!(prompt_command.action, PromptAction::Run(_))
        }
        Some(Command::Run(_) | Command::Agent(_) | Command::Chat(_)) | None => true,
    }
}

/// Runs the commands `needs_async_runtime` classifies as synchronous,
/// without any async runtime behind them.
pub(crate) fn run_blocking(cli: Cli) -> Result<()> {
    let config_source = ConfigSource::from(&cli);
    match cli.command {
        Some(Command::Lint(lint_args)) => lint::run(lint_args, config_source),
        Some(Command::Models(models_args)) => {
            if models_args.remote {
                bail!("internal error: `models --remote` must run on the async path");
            }
            crate::models::run_local(models_args, config_source)
        }
        Some(Command::Completions(completions_args)) => {
            docgen::generate_completions(completions_args);
            Ok(())
        }
        Some(Command::Man(man_args)) => docgen::generate_man_pages(man_args),
        Some(Command::Init(init_args)) => crate::init::run(init_args),
        Some(Command::Sessions(sessions_command)) => crate::session::run(sessions_command),
        Some(Command::Prompt(prompt_command)) => match prompt_command.action {
            PromptAction::List => crate::prompt::list(&config::load_config(&config_source)?),
            PromptAction::Run(_) => {
                bail!("internal error: `prompt run` must run on the async path")
            }
        },
        Some(Command::History(history_args)) => crate::history::run(history_args),
        Some(Command::Run(_) | Command::Agent(_) | Command::Chat(_)) | None => {
            bail!("internal error: an async command reached run_blocking")
        }
    }
}

/// Resolves chat mode's system prompt: `--system` text, else `--system-file`
/// contents, else `default.system` from lait.config.yml (`--system` and
/// `--system-file` conflict at the clap level, so their order here never
/// actually decides anything).
pub(crate) fn resolve_system_prompt(
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
pub(crate) fn resolve_chat_settings(
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
        shared.endpoint.base_url.clone(),
        shared.endpoint.api_key.clone(),
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
pub(crate) fn load_session_history(
    session_name: Option<&str>,
) -> Result<Vec<ChatCompletionRequestMessage>> {
    match session_name {
        Some(name) => session::to_request_messages(&session::load(name)?),
        None => Ok(Vec::new()),
    }
}

/// Runs a single-shot chat request with an already-resolved `prompt` — see
/// `run_chat_or_repl`, the only caller, for how `prompt` was resolved (a
/// CLI argument and/or piped stdin).
async fn run_chat(chat: ChatArgs, prompt: String, config_source: ConfigSource) -> Result<()> {
    let file_config = Arc::new(config::load_config(&config_source)?);

    // `-p`/`--prompt-name` renders a named `prompts:` template against
    // `prompt` (which, for this path, is really the template's `{{ input }}`
    // rather than literal text to send) before anything else touches it —
    // `--file` attachments below still append to the *rendered* text, the
    // same way they'd append to a plain prompt.
    let (prompt, prompt_model_fallback) = match &chat.prompt_name {
        Some(name) => prompt::render_named(name, &prompt, &chat.var.var, &file_config)?,
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
    let env = AppContext::new(Arc::clone(&file_config));

    // `--quiet` keeps the response body and drops every note around it.
    let show_reasoning = chat.shared.show_reasoning && !chat.quiet;
    let show_usage = chat.shared.reporting.show_usage && !chat.quiet;
    let render_enabled = chat.output.render || file_config.default.render.unwrap_or(false);
    // `-o -` is an explicit "stdout", the same as no `-o` at all.
    let output_path = chat
        .output
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
            chat.shared.reporting.no_history,
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
            let body = response::render_response(&response, chat.output.json, false)?;
            report::emit_output(&body, Some(path), false)?;
        }
        None => {
            let output = response::render_response(&response, chat.output.json, show_reasoning)?;
            // `--json`'s output is machine-readable and never rendered as
            // Markdown; `chat.stream`'s branch above already returned before
            // reaching here, so `--render` never has to reckon with a
            // partial streamed response either — see `render::maybe_render`.
            report::emit_output(&output, None, !chat.output.json && render_enabled)?;
        }
    }
    let content = response::content_text(&response);
    finish_chat_turn(
        chat.shared.session.as_deref(),
        chat.shared.reporting.no_history,
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

/// Runs `lait prompt run <NAME> [INPUT]` (`lait prompt list` is handled
/// separately, synchronously, by `prompt::list` — see `needs_async_runtime`/
/// `run_blocking`): renders the named prompt (see `prompt::render_named`)
/// and sends the result as a plain, tool-free, non-streamed request. This
/// subcommand form is intentionally narrower than `-p`/`--prompt-name` on
/// the main chat invocation (no `--model`/`--stream`/`--mcp`/... overrides —
/// see `docs/usage/ja/prompts.md`); reach for `-p` when those are needed.
/// `-o`/`--render`/`--json`/`--show-usage`/`--no-history` work the same as
/// every other `run_*` entry point (see `cli::OutputArgs`).
async fn run_prompt(args: PromptRunArgs, config_source: ConfigSource) -> Result<()> {
    let file_config = Arc::new(config::load_config(&config_source)?);

    let raw_input = resolve_input_with_stdin(args.input.clone())?
        .ok_or_else(|| anyhow!("an INPUT is required; provide one or pipe input via stdin"))?;
    let (prompt_text, prompt_model) =
        prompt::render_named(&args.name, &raw_input, &args.var.var, &file_config)?;

    // `.filter` folds a blank-but-present `model:` into the same "none
    // configured" branch below, so it gets this prompt-specific hint
    // ('prompts.<name>.model'/'default.model') instead of
    // `config::resolve_model`'s generic "model name must not be empty" —
    // `resolve_request_settings` still enforces the latter as a backstop for
    // any model name reaching it by another path, but this is the more
    // useful message for the one a `lait prompt` author would actually see.
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

    let env = AppContext::new(Arc::clone(&file_config));
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
    report::emit_run_output(&output, env.usage.total(), &args.output, &file_config)?;
    report::finish_run(
        report::RunRecord {
            kind: "prompt",
            model: Some(&settings.resolved_model.model_id),
            prompt: &prompt_text,
            response: &output,
        },
        args.reporting.no_history,
        &file_config,
        &env.usage,
        args.reporting.show_usage,
    )
}

async fn run_agent(args: AgentRunArgs, config_source: ConfigSource) -> Result<()> {
    let raw_input = resolve_input_with_stdin(args.input.clone())?
        .ok_or_else(|| anyhow!("an INPUT is required; provide one or pipe input via stdin"))?;
    let agent_file = agent::load_agent(&args.file)?;
    let canonical_agent_path = std::fs::canonicalize(&args.file).with_context(|| {
        format!(
            "failed to resolve agent file path '{}'",
            args.file.display()
        )
    })?;
    let file_config = Arc::new(config::load_config(&config_source)?);

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

    let env = AppContext::new(Arc::clone(&file_config));
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
    report::emit_run_output(&output, env.usage.total(), &args.output, &file_config)?;
    report::finish_run(
        report::RunRecord {
            kind: "agent",
            model: Some(&settings.resolved_model.model_id),
            prompt: &raw_input,
            response: &output,
        },
        args.reporting.no_history,
        &file_config,
        &env.usage,
        args.reporting.show_usage,
    )
}

async fn run_workflow(run_args: RunArgs, config_source: ConfigSource) -> Result<()> {
    let prompt = resolve_input_with_stdin(run_args.prompt.clone())?
        .ok_or_else(|| anyhow!("a PROMPT is required; provide one or pipe input via stdin"))?;
    let mut wf = workflow::load_workflow(&run_args.file)?;
    let file_config = Arc::new(config::load_config(&config_source)?);

    announce_named_file("==>", wf.name.as_deref(), wf.description.as_deref());

    let scope = WorkflowScope::top_level(&mut wf, &run_args.file)?;
    let env = AppContext::new(Arc::clone(&file_config));
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
    report::emit_run_output(
        &current_input,
        env.usage.total(),
        &run_args.output,
        &file_config,
    )?;
    report::finish_run(
        // A workflow can touch several models across its steps, so no
        // single `model` is recorded here — see `history::HistoryEntry::model`.
        report::RunRecord {
            kind: "workflow",
            model: None,
            prompt: &initial_prompt,
            response: &current_input,
        },
        run_args.reporting.no_history,
        &file_config,
        &env.usage,
        run_args.reporting.show_usage,
    )
}
