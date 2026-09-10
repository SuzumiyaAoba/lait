//! Top-level command dispatch: turns a parsed `cli::Command` into a call
//! into the module that actually implements it (`lint::run`,
//! `workflow_run::run_workflow`, `repl::run`, ...). This module should stay
//! pure dispatch — building a chat turn's settings/history/cache policy
//! lives in `chat` instead (see `chat.rs`'s own doc comment for why that
//! split exists), and `run`/`run_chat`/`run_prompt`/`run_agent` below only
//! wire those pieces together for their one entry point each.
//!
//! Every `Command` variant is classified as needing an async runtime or not
//! (`needs_async_runtime`) before `main` decides whether to start one at
//! all; `run` (async path) and `run_blocking` (sync path) then each match
//! over `Command` again to dispatch. All three matches must agree, and nothing
//! in the type system enforces that today — a variant routed to the wrong
//! path fails at runtime via one of the `internal error: ...` `bail!`s below
//! rather than at compile time (see the design plan's B3 for the planned
//! fix: a `Dispatch`/`classify` type that makes misrouting a compile error).

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    agent, attachment, chat, checkpoint,
    cli::{AgentAction, ChatArgs, ChatReplArgs, Cli, Command, PromptAction},
    cli::{AgentRunArgs, GraphArgs, GraphFormat, PromptRunArgs},
    cli::{SkillAction, WorkflowAction},
    config::{self, ConfigSource, ModelMap},
    docgen, doctor,
    engine::{
        AgentTurn, AppServices, CapabilityOverrides, PromptTurn, RunContext, SamplingOverrides,
        agent_file_settings, call_agent, resolve_request_settings,
    },
    history, lint, prompt, repl, report, response, schema, skill, subagent, template, test_run,
    usage,
    workflow::{self, exec::announce_named_file},
};

mod workflow_run;
use workflow_run::run_workflow;

pub(crate) async fn run(cli: Cli) -> Result<()> {
    // Built once per invocation and passed as the root source to each
    // RunContext below. Each async handler arms `signal::spawn_handler` at
    // the point where it starts using the token; the REPL watches that root
    // while reading and running turns so cleanup also covers Ctrl-C there.
    let cancel = tokio_util::sync::CancellationToken::new();

    let config_source = ConfigSource::from(&cli);
    // `--cache`/`--no-cache` are global flags on `Cli` itself (see
    // `cli::Cli`), so they must be read before `cli.command` is moved into
    // the match below — same reason `config_source` is built up front here
    // rather than re-derived from `cli` at each call site.
    let cache_override = cache_override(cli.cache, cli.no_cache);
    let approve_tools = cli.approve_tools;
    match cli.command {
        Some(Command::Run(run_args)) => {
            run_workflow(
                run_args,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
        Some(Command::Agent(agent_command)) => match agent_command.action {
            AgentAction::Run(args) => {
                run_agent(args, config_source, cache_override, approve_tools, cancel).await
            }
            AgentAction::List => bail!("internal error: `agent list` must run on the sync path"),
        },
        Some(Command::Lint(lint_args)) => lint::run(lint_args, config_source),
        Some(Command::Models(models_args)) => {
            crate::models::run(models_args, config_source, Some(cancel)).await
        }
        Some(Command::Completions(completions_args)) => {
            docgen::generate_completions(completions_args);
            Ok(())
        }
        Some(Command::Man(man_args)) => docgen::generate_man_pages(man_args),
        Some(Command::Init(init_args)) => crate::init::run(init_args),
        Some(Command::Sessions(sessions_command)) => crate::session::run(sessions_command),
        Some(Command::Chat(chat_repl_args)) => {
            repl::run(
                chat_repl_args,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
        Some(Command::Prompt(prompt_command)) => match prompt_command.action {
            PromptAction::List => {
                bail!("internal error: `prompt list` must run on the sync path")
            }
            PromptAction::Run(run_args) => {
                run_prompt(
                    run_args,
                    config_source,
                    cache_override,
                    approve_tools,
                    cancel,
                )
                .await
            }
        },
        Some(Command::History(history_args)) => history::run(history_args),
        Some(Command::Graph(_)) => bail!("internal error: `graph` must run on the sync path"),
        Some(Command::Workflow(_)) => {
            bail!("internal error: `workflow list` must run on the sync path")
        }
        Some(Command::Skill(_)) => bail!("internal error: `skill list` must run on the sync path"),
        Some(Command::Runs(_)) => bail!("internal error: `runs` must run on the sync path"),
        Some(Command::Cache(_)) => bail!("internal error: `cache` must run on the sync path"),
        Some(Command::Schema(_)) => bail!("internal error: `schema` must run on the sync path"),
        Some(Command::Doctor(doctor_args)) => {
            doctor::run(doctor_args, config_source, Some(cancel)).await
        }
        Some(Command::Compare(compare_args)) => {
            crate::compare::run(compare_args, config_source, cache_override, cancel).await
        }
        Some(Command::Test(test_args)) => test_run::run(test_args, config_source, cancel).await,
        Some(Command::Eval(eval_args)) => crate::eval::run(eval_args, config_source, cancel).await,
        None => {
            run_chat_or_repl(
                cli.chat,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
    }
}

/// Resolves `--cache`/`--no-cache` (mutually exclusive at the clap level, see
/// `cli::Cli`) into the `Option<bool>` `chat::resolve_cache_settings` expects:
/// `Some(true)`/`Some(false)` when either flag was passed, `None` when
/// neither was, letting `default.cache` in lait.config.yml decide.
fn cache_override(cache: bool, no_cache: bool) -> Option<bool> {
    if cache {
        Some(true)
    } else if no_cache {
        Some(false)
    } else {
        None
    }
}

/// The bare-invocation entry point (`lait [OPTIONS] [PROMPT]`, no
/// subcommand): sends a single-shot chat request when a prompt is available
/// (an argument or piped stdin — see `chat::resolve_input_with_stdin_cancellable`), or, when
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
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use std::io::IsTerminal;

    // The REPL installs its own listener once it starts. Every other bare
    // invocation must install one before the cancellable initial stdin read
    // (or before `run_chat`'s config load).
    let enters_repl = chat.prompt.is_none() && std::io::stdin().is_terminal();
    if !enters_repl {
        crate::signal::spawn_handler(cancel.clone());
    }
    match chat::resolve_input_with_stdin_cancellable(chat.prompt.clone(), Some(cancel.clone()))
        .await?
    {
        Some(prompt) => {
            run_chat(
                chat,
                prompt,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
        None if std::io::stdin().is_terminal() => {
            repl::run(
                ChatReplArgs {
                    shared: chat.shared,
                },
                config_source,
                cache_override,
                approve_tools,
                cancel,
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
            | Command::Runs(_)
            | Command::Cache(_)
            | Command::Schema(_),
        ) => false,
        Some(Command::Models(models_args)) => models_args.remote,
        Some(Command::Prompt(prompt_command)) => {
            matches!(prompt_command.action, PromptAction::Run(_))
        }
        Some(Command::Agent(agent_command)) => {
            matches!(agent_command.action, AgentAction::Run(_))
        }
        Some(
            Command::Run(_)
            | Command::Chat(_)
            | Command::Doctor(_)
            | Command::Compare(_)
            | Command::Test(_)
            | Command::Eval(_),
        )
        | None => true,
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
            AgentAction::List => subagent::list(&config::load_config(&config_source)?),
            AgentAction::Run(_) => {
                bail!("internal error: `agent run` must run on the async path")
            }
        },
        Some(Command::Workflow(workflow_command)) => match workflow_command.action {
            WorkflowAction::List => workflow::list(&config::load_config(&config_source)?),
        },
        Some(Command::Skill(skill_command)) => match skill_command.action {
            SkillAction::List => skill::list(&config::load_config(&config_source)?),
        },
        Some(Command::Runs(runs_command)) => checkpoint::run(runs_command),
        Some(Command::Cache(cache_command)) => crate::cache::run(cache_command),
        Some(Command::Schema(schema_args)) => crate::schema::run(schema_args),
        Some(
            Command::Run(_)
            | Command::Chat(_)
            | Command::Doctor(_)
            | Command::Compare(_)
            | Command::Test(_)
            | Command::Eval(_),
        )
        | None => {
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

/// Runs a single-shot chat request with an already-resolved `prompt` — see
/// `run_chat_or_repl`, the only caller, for how `prompt` was resolved (a
/// CLI argument and/or piped stdin).
async fn run_chat(
    chat: ChatArgs,
    prompt: String,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let file_config =
        Arc::new(config::load_config_cancellable(&config_source, Some(cancel.clone())).await?);

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
        chat::resolve_chat_settings(&chat.shared, prompt_model_fallback.as_deref(), &file_config)?;

    let response_format = chat
        .json_schema
        .as_deref()
        .map(|path| schema::load_json_schema(path, &chat.schema_name))
        .transpose()?;

    let system_prompt =
        chat::resolve_system_prompt(&chat.shared, &file_config, Some(cancel.clone())).await?;
    let image_urls = attachment::resolve_image_urls(&chat.images).await?;
    let session_history = chat::load_session_history(chat.shared.session.as_deref())?;
    let (cache_enabled, cache_ttl) = chat::resolve_cache_settings(cache_override, &file_config);
    let services = Arc::new(AppServices::new(Arc::clone(&file_config)));
    let env = RunContext::new(Arc::clone(&services), cancel)
        .with_cache(cache_enabled, cache_ttl)
        .with_approve_tools(approve_tools);

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
        let outcome = services
            .finish(settings.complete_stream(
                &env,
                &[],
                turn,
                response_format,
                show_usage,
                show_reasoning,
                output_path,
                Some(env.operation_token()),
            ))
            .await?;
        // Streamed usage arrives on the final chunk rather than through
        // `complete`; feed it into the same tally so both chat paths share
        // one summary format and so `env.usage.total()` below reflects it.
        if let Some(usage) = outcome.usage {
            env.usage.record(&settings.usage_label, usage);
        }
        chat::finish_chat_turn(
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

    let response = services
        .finish(settings.complete(
            &env,
            &[],
            turn,
            response_format,
            Some(env.operation_token()),
        ))
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
    chat::finish_chat_turn(
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
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    let file_config =
        Arc::new(config::load_config_cancellable(&config_source, Some(cancel.clone())).await?);

    let raw_input =
        chat::resolve_input_with_stdin_cancellable(args.input.clone(), Some(cancel.clone()))
            .await?
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

    let (cache_enabled, cache_ttl) = chat::resolve_cache_settings(cache_override, &file_config);
    let services = Arc::new(AppServices::new(Arc::clone(&file_config)));
    let env = RunContext::new(Arc::clone(&services), cancel)
        .with_cache(cache_enabled, cache_ttl)
        .with_approve_tools(approve_tools);
    let response = services
        .finish(settings.complete(
            &env,
            &[],
            PromptTurn::simple(None, &prompt_text),
            None,
            Some(env.operation_token()),
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
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    let raw_input =
        chat::resolve_input_with_stdin_cancellable(args.input.clone(), Some(cancel.clone()))
            .await?
            .ok_or_else(|| anyhow!("an INPUT is required; provide one or pipe input via stdin"))?;
    let agent_file = agent::load_agent_cancellable(&args.file, Some(cancel.clone())).await?;
    let canonical_agent_path = crate::async_io::canonicalize(&args.file, Some(cancel.clone()))
        .await
        .with_context(|| {
            format!(
                "failed to resolve agent file path '{}'",
                args.file.display()
            )
        })?;
    let file_config =
        Arc::new(config::load_config_cancellable(&config_source, Some(cancel.clone())).await?);

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

    let (cache_enabled, cache_ttl) = chat::resolve_cache_settings(cache_override, &file_config);
    let services = Arc::new(AppServices::new(Arc::clone(&file_config)));
    let env = RunContext::new(Arc::clone(&services), cancel)
        .with_cache(cache_enabled, cache_ttl)
        .with_approve_tools(approve_tools);
    let output = services
        .finish(call_agent(
            &agent_file,
            &settings,
            &env,
            AgentTurn::simple(&input, &raw_input),
            &workflow::StepOutputs::new(),
            std::slice::from_ref(&canonical_agent_path),
            Some(env.operation_token()),
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
