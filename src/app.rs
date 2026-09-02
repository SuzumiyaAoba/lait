use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::ChatCompletionRequestMessage;

use crate::{
    agent, attachment, checkpoint,
    cli::{AgentAction, ChatArgs, ChatReplArgs, Cli, Command, PromptAction, RunArgs},
    cli::{AgentRunArgs, GraphArgs, GraphFormat, PromptRunArgs, SharedChatArgs},
    cli::{SkillAction, WorkflowAction},
    config::{self, ConfigFile, ConfigSource, ModelMap},
    docgen,
    engine::{
        AgentTurn, AppContext, CapabilityOverrides, PromptTurn, RequestSettings, SamplingOverrides,
        agent_file_settings, call_agent, resolve_request_settings, stream_response,
    },
    history, lint, prompt, repl, report, response, schema, session, skill, subagent, template,
    usage,
    workflow::{
        self, WorkflowScope,
        exec::{Flow, RunStepsFrame, StepsOutcome, announce_named_file, run_steps},
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
    // Built once per invocation and threaded into every async command below
    // via `AppContext::with_cancel`; each single-shot handler
    // (`run_workflow`/`run_agent`/`run_prompt`/`run_chat`) arms
    // `signal::spawn_handler` itself, right where it actually starts using
    // the token — *not* here, and deliberately never for `repl::run`. A
    // `CancellationToken` fires once and stays cancelled forever, but the
    // REPL reuses one `AppContext` across many turns: arming Ctrl-C
    // process-wide would make the first Ctrl-C (meant to interrupt just the
    // in-flight turn) silently break every turn after it, with no visible
    // effect on the one it was pressed during. `lait chat` keeps its
    // pre-existing Ctrl-C behavior (default disposition: an immediate kill)
    // until the REPL has its own per-turn cancellation scope.
    let cancel = tokio_util::sync::CancellationToken::new();

    let config_source = ConfigSource::from(&cli);
    match cli.command {
        Some(Command::Run(run_args)) => run_workflow(run_args, config_source, cancel).await,
        Some(Command::Agent(agent_command)) => match agent_command.action {
            AgentAction::Run(args) => run_agent(args, config_source, cancel).await,
            AgentAction::List => bail!("internal error: `agent list` must run on the sync path"),
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
            PromptAction::Run(run_args) => run_prompt(run_args, config_source, cancel).await,
        },
        Some(Command::History(history_args)) => history::run(history_args),
        Some(Command::Graph(_)) => bail!("internal error: `graph` must run on the sync path"),
        Some(Command::Workflow(_)) => {
            bail!("internal error: `workflow list` must run on the sync path")
        }
        Some(Command::Skill(_)) => bail!("internal error: `skill list` must run on the sync path"),
        Some(Command::Runs(_)) => bail!("internal error: `runs` must run on the sync path"),
        None => run_chat_or_repl(cli.chat, config_source, cancel).await,
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
async fn run_chat_or_repl(
    chat: ChatArgs,
    config_source: ConfigSource,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use std::io::IsTerminal;

    match resolve_input_with_stdin(chat.prompt.clone())? {
        Some(prompt) => run_chat(chat, prompt, config_source, cancel).await,
        // `repl::run` deliberately does not receive `cancel`: a
        // `CancellationToken` fires once and stays cancelled forever, but
        // the REPL reuses one `AppContext` across many turns — wiring a
        // single process-wide token in would make the *first* Ctrl-C (meant
        // to interrupt just the in-flight turn) permanently break every
        // turn after it. `lait chat` keeps its pre-existing Ctrl-C behavior
        // until the REPL has its own per-turn cancellation scope.
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
            | Command::History(_)
            | Command::Graph(_)
            | Command::Workflow(_)
            | Command::Skill(_)
            | Command::Runs(_),
        ) => false,
        Some(Command::Models(models_args)) => models_args.remote,
        Some(Command::Prompt(prompt_command)) => {
            matches!(prompt_command.action, PromptAction::Run(_))
        }
        Some(Command::Agent(agent_command)) => {
            matches!(agent_command.action, AgentAction::Run(_))
        }
        Some(Command::Run(_) | Command::Chat(_)) | None => true,
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
        Some(Command::Graph(graph_args)) => run_graph(graph_args),
        Some(Command::Agent(agent_command)) => match agent_command.action {
            AgentAction::List => {
                let config_dir = config::resolve_config_dir(&config_source)?;
                subagent::list(&config::load_config(&config_source)?, config_dir.as_deref())
            }
            AgentAction::Run(_) => {
                bail!("internal error: `agent run` must run on the async path")
            }
        },
        Some(Command::Workflow(workflow_command)) => match workflow_command.action {
            WorkflowAction::List => {
                let config_dir = config::resolve_config_dir(&config_source)?;
                workflow::list(&config::load_config(&config_source)?, config_dir.as_deref())
            }
        },
        Some(Command::Skill(skill_command)) => match skill_command.action {
            SkillAction::List => {
                let config_dir = config::resolve_config_dir(&config_source)?;
                skill::list(&config::load_config(&config_source)?, config_dir.as_deref())
            }
        },
        Some(Command::Runs(runs_command)) => checkpoint::run(runs_command),
        Some(Command::Run(_) | Command::Chat(_)) | None => {
            bail!("internal error: an async command reached run_blocking")
        }
    }
}

/// Runs `lait graph`: parses/validates the workflow file the same way `lait
/// run`/`lait lint` do, then prints its Mermaid/DOT control-flow graph. Pure
/// local work (no `lait.config.yml`, no model resolution) — a workflow's
/// `models:`/`default:` never affect what the graph looks like.
fn run_graph(graph_args: GraphArgs) -> Result<()> {
    let wf = workflow::load_workflow(&graph_args.file)?;
    let format = match graph_args.format {
        GraphFormat::Mermaid => workflow::graph::GraphFormat::Mermaid,
        GraphFormat::Dot => workflow::graph::GraphFormat::Dot,
    };
    print!("{}", workflow::graph::render(&wf, format)?);
    Ok(())
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
async fn run_chat(
    chat: ChatArgs,
    prompt: String,
    config_source: ConfigSource,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
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
    let env = AppContext::new(Arc::clone(&file_config)).with_cancel(cancel);

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
                stream_response(stream, show_reasoning, output_path, env.cancel.clone()).await
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
async fn run_prompt(
    args: PromptRunArgs,
    config_source: ConfigSource,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
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

    let env = AppContext::new(Arc::clone(&file_config)).with_cancel(cancel);
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

async fn run_agent(
    args: AgentRunArgs,
    config_source: ConfigSource,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
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

    let env = AppContext::new(Arc::clone(&file_config)).with_cancel(cancel);
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

/// Every top-level step's label, by position: this site's own label (see
/// `FlowStep::label`) if set, else `step-<position>` (1-based). Deliberately
/// *not* the same value `run_steps`' own progress-counter fallback would
/// produce for an unlabeled router site — that counter only exists once a
/// run is actually executing (it also counts nested steps), whereas this
/// only needs to name each top-level position stably, before anything has
/// run, so `checkpoint::check_resumable` can detect whether the step
/// sequence changed since a checkpoint was written.
fn top_level_step_labels(steps: &[workflow::FlowStep]) -> Vec<String> {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| step.label_or(index + 1))
        .collect()
}

async fn run_workflow(
    run_args: RunArgs,
    config_source: ConfigSource,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    let file_config = Arc::new(config::load_config(&config_source)?);
    let config_dir = config::resolve_config_dir(&config_source)?;
    let resolved_file =
        workflow::resolve_run_target(&run_args.file, &file_config, config_dir.as_deref());
    let workflow_path = resolved_file.display().to_string();

    let resumed = run_args
        .resume
        .as_deref()
        .map(checkpoint::load)
        .transpose()?;
    if let Some(resumed) = &resumed {
        if resumed.workflow_path != workflow_path {
            bail!(
                "run '{}' was checkpointed against workflow '{}', not '{workflow_path}'; pass \
                 the same FILE to resume it",
                resumed.run_id,
                resumed.workflow_path,
            );
        }
        if resumed.status == checkpoint::RunStatus::Completed {
            bail!(
                "run '{}' already completed; nothing to resume",
                resumed.run_id
            );
        }
    }

    let mut wf = workflow::load_workflow(&resolved_file)?;
    announce_named_file("==>", wf.name.as_deref(), wf.description.as_deref());
    let scope = WorkflowScope::top_level(&mut wf, &resolved_file)?;
    let top_level_labels = top_level_step_labels(&wf.steps);

    let (initial_prompt, vars, start_index, start_counter, start_input, start_steps_outputs) =
        match &resumed {
            Some(resumed) => {
                checkpoint::check_resumable(&top_level_labels, resumed)?;
                eprintln!(
                    "==> resuming run '{}' from step {}/{}",
                    resumed.run_id,
                    resumed.completed_index + 1,
                    top_level_labels.len(),
                );
                let vars = if run_args.var.var.is_empty() {
                    resumed.vars.clone()
                } else {
                    workflow::build_vars(&run_args.var.var)?
                };
                (
                    resumed.initial_prompt.clone(),
                    vars,
                    resumed.completed_index,
                    resumed.counter,
                    resumed.current_input.clone(),
                    resumed.steps_outputs.clone(),
                )
            }
            None => {
                let prompt =
                    resolve_input_with_stdin(run_args.prompt.clone())?.ok_or_else(|| {
                        anyhow!("a PROMPT is required; provide one or pipe input via stdin")
                    })?;
                let vars = workflow::build_vars(&run_args.var.var)?;
                (
                    prompt.clone(),
                    vars,
                    0,
                    0,
                    prompt,
                    workflow::StepOutputs::new(),
                )
            }
        };
    let run_id = match &resumed {
        Some(resumed) => resumed.run_id.clone(),
        None => checkpoint::generate_run_id(),
    };

    if run_args.dry_run {
        return workflow::dryrun::print_plan(&wf, &scope, &file_config, &initial_prompt, &vars);
    }

    // `--resume` implies `--checkpoint`: a run started with `--checkpoint`
    // stays checkpointed across a resume without the flag needing to be
    // repeated.
    let checkpointing = run_args.checkpoint || resumed.is_some();

    // `default.workflow_timeout` bounds this run's total wall-clock time,
    // distinct from a node's own `timeout:` (which bounds one step). Built
    // as a child of the process-wide Ctrl-C token (mirroring how
    // `execute_step_with_retry` derives a node's own timeout token from its
    // caller's) so either source cancels the same run token every
    // downstream call already watches — a spawned sleep-then-cancel task
    // fires it once the budget is exhausted, same as a step's own timeout.
    let run_cancel = cancel.child_token();
    if let Some(seconds) = scope.defaults.workflow_timeout {
        let timeout_cancel = run_cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            // The step that's actually cancelled only ever sees/reports a
            // generic "cancelled" error (the same one a Ctrl-C produces) —
            // this is the one place that knows *why*, so it's the one place
            // that can say so before the workflow's own error obscures it.
            eprintln!("lait: 'default.workflow_timeout' ({seconds}s) exceeded; cancelling the run");
            timeout_cancel.cancel();
        });
    }

    let env = AppContext::new(Arc::clone(&file_config))
        .with_vars(vars.clone())
        .with_cancel(run_cancel);
    // `completed_index` ends at `wf.steps.len()` when the loop runs to
    // completion, or at the position right after whichever step set
    // `flow != Flow::Continue` (a `stop: true`) when it ends early — either
    // way, the count of top-level steps actually executed this run. Declared
    // outside the `async` block (mutated from within, read after it) so its
    // final value is available for the "completed" checkpoint below without
    // smuggling it out through the block's own `Result`.
    let mut completed_index = start_index;
    let run_result: Result<(String, usize, workflow::StepOutputs)> = env
        .finish(async {
            let mut current_input = start_input;
            let mut steps_outputs = start_steps_outputs;
            let mut counter = start_counter;
            for (index, step) in wf.steps.iter().enumerate().skip(start_index) {
                // `run_steps` takes `current_input`/`steps_outputs` by value,
                // so a failing step leaves nothing to save afterward — clone
                // them beforehand (they're re-recording unchanged state, plus
                // any new `vars` this invocation brought in, since the step
                // itself never produced a new output) rather than trying to
                // reconstruct them post-failure.
                let (unchanged_input, unchanged_steps_outputs) = if checkpointing {
                    (Some(current_input.clone()), Some(steps_outputs.clone()))
                } else {
                    (None, None)
                };
                let step_outcome = run_steps(
                    std::slice::from_ref(step),
                    current_input,
                    steps_outputs,
                    RunStepsFrame {
                        scope: &scope,
                        env: &env,
                        start_counter: counter,
                        progress_prefix: "",
                        cancellation: env.cancel.clone(),
                    },
                )
                .await;
                let StepsOutcome {
                    output,
                    counter: new_counter,
                    flow,
                    steps_outputs: new_steps_outputs,
                } = match step_outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        if checkpointing {
                            // A save failure here must not shadow `error`
                            // (the workflow's own failure — e.g. "workflow
                            // execution was cancelled" from a Ctrl-C, which
                            // the caller still needs to see and propagate)
                            // — log it to stderr and keep going instead.
                            let save_result = checkpoint::save(&checkpoint::Checkpoint {
                                run_id: run_id.clone(),
                                workflow_path: workflow_path.clone(),
                                initial_prompt: initial_prompt.clone(),
                                vars: vars.clone(),
                                top_level_labels: top_level_labels.clone(),
                                completed_index,
                                counter,
                                current_input: unchanged_input
                                    .expect("cloned above when checkpointing"),
                                steps_outputs: unchanged_steps_outputs
                                    .expect("cloned above when checkpointing"),
                                status: checkpoint::RunStatus::Failed,
                            });
                            match save_result {
                                Ok(()) => eprintln!(
                                    "note: run checkpointed as '{run_id}'; resume with `lait run \
                                     {} --resume {run_id}`",
                                    run_args.file.display(),
                                ),
                                Err(save_error) => eprintln!(
                                    "warning: failed to save checkpoint for run '{run_id}': \
                                     {save_error:#}"
                                ),
                            }
                        }
                        return Err(error);
                    }
                };
                current_input = output;
                counter = new_counter;
                steps_outputs = new_steps_outputs;
                completed_index = index + 1;
                if checkpointing {
                    checkpoint::save(&checkpoint::Checkpoint {
                        run_id: run_id.clone(),
                        workflow_path: workflow_path.clone(),
                        initial_prompt: initial_prompt.clone(),
                        vars: vars.clone(),
                        top_level_labels: top_level_labels.clone(),
                        completed_index,
                        counter,
                        current_input: current_input.clone(),
                        steps_outputs: steps_outputs.clone(),
                        status: checkpoint::RunStatus::Failed,
                    })?;
                }
                if flow != Flow::Continue {
                    break;
                }
            }
            Ok((current_input, counter, steps_outputs))
        })
        .await;
    let (current_input, counter, steps_outputs) = run_result?;

    if checkpointing {
        checkpoint::save(&checkpoint::Checkpoint {
            run_id,
            workflow_path,
            initial_prompt: initial_prompt.clone(),
            vars,
            top_level_labels,
            completed_index,
            counter,
            current_input: current_input.clone(),
            steps_outputs,
            status: checkpoint::RunStatus::Completed,
        })?;
    }

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
