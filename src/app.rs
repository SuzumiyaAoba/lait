//! Top-level command dispatch: turns a parsed `cli::Command` into a call
//! into the module that actually implements it (`lint::run`,
//! `workflow_run::run_workflow`, `repl::run`, ...). This module should stay
//! pure dispatch — building a chat turn's settings/history/cache policy
//! lives in `chat` instead (see `chat.rs`'s own doc comment for why that
//! split exists), and `run`/`run_chat`/`run_prompt`/`run_agent` below only
//! wire those pieces together for their one entry point each.
//!
//! [`classify`] sorts every `Command` variant (plus the no-subcommand bare
//! invocation) into [`SyncCommand`] or [`AsyncCommand`] once, in `main`: the
//! sync/async split used to be encoded separately in three places (a
//! `needs_async_runtime` predicate `main` consulted before deciding whether
//! to start a Tokio runtime at all, plus [`run`] and [`run_blocking`] each
//! re-matching over `Command` to dispatch), kept in agreement only by
//! convention and twelve `bail!("internal error: ... must run on the
//! {sync,async} path")` calls if they ever drifted. `run`/`run_blocking` now
//! take `AsyncCommand`/`SyncCommand` directly rather than the raw `Cli`, so a
//! variant classified as sync literally cannot reach `run`'s async handlers
//! (or vice versa) — the type system rejects it at `main`'s single call
//! site, not at runtime.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

use crate::{
    agent, attachment, chat, checkpoint,
    cli::{
        AgentAction, AgentCommand, AgentRunArgs, CacheCommand, ChatArgs, ChatReplArgs, Command,
        CompareArgs, CompletionsArgs, DoctorArgs, EvalArgs, GraphArgs, GraphFormat, HistoryArgs,
        InitArgs, LintArgs, ManArgs, ModelsArgs, PromptAction, PromptCommand, PromptRunArgs,
        RunArgs, RunsCommand, SchemaArgs, SessionsCommand, SkillAction, SkillCommand, TestArgs,
        WorkflowAction, WorkflowCommand,
    },
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

/// Every subcommand (and the bare, no-subcommand invocation) that never
/// awaits anything — no model request, no MCP connection — grouped by
/// [`classify`] so [`run_blocking`] can dispatch to it without a Tokio
/// runtime. `main` skips building one entirely for these, which matters for
/// `Completions` in particular: shell startup files run it on every new
/// shell, where runtime-startup cost is felt directly.
pub(crate) enum SyncCommand {
    Lint(LintArgs),
    ModelsLocal(ModelsArgs),
    Completions(CompletionsArgs),
    Man(ManArgs),
    Init(InitArgs),
    Sessions(SessionsCommand),
    PromptList,
    History(HistoryArgs),
    Graph(GraphArgs),
    AgentList,
    WorkflowList,
    SkillList,
    Runs(RunsCommand),
    Cache(CacheCommand),
    Schema(SchemaArgs),
}

/// Every subcommand (and the bare invocation) that awaits a model request or
/// MCP/subagent work — see [`SyncCommand`] for the complementary half and
/// this module's doc comment for why the split exists as a type rather than
/// a predicate.
pub(crate) enum AsyncCommand {
    Run(RunArgs),
    AgentRun(AgentRunArgs),
    ModelsRemote(ModelsArgs),
    Chat(ChatReplArgs),
    PromptRun(PromptRunArgs),
    Doctor(DoctorArgs),
    Compare(CompareArgs),
    Test(TestArgs),
    Eval(EvalArgs),
    /// The no-subcommand invocation (`lait [OPTIONS] [PROMPT]`). Carries no
    /// payload here — unlike every other variant, its arguments
    /// (`cli::Cli::chat`) live directly on `Cli` rather than on a `Command`
    /// variant (there is no `Command` value at all when no subcommand was
    /// given), so `main` threads `cli.chat` to [`run`] alongside this tag
    /// instead of `classify` trying to manufacture one from an
    /// `Option<Command>` that structurally cannot carry it.
    Bare,
}

/// The result of sorting one parsed `command` into the sync/async split —
/// see this module's doc comment. Takes `Option<Command>` (not the whole
/// `Cli`) because that is all a `Command` variant's own classification ever
/// depends on; the caller still owns every other `Cli` field (`chat`,
/// `no_config`, `cache`, ...) throughout, since only the `command` field is
/// moved into this call.
pub(crate) enum Dispatch {
    Sync(SyncCommand),
    Async(AsyncCommand),
}

/// Classifies a parsed `Cli::command` into [`Dispatch::Sync`]/
/// [`Dispatch::Async`]. Two subcommands classify by a field on their own
/// payload rather than by variant alone: `Models(remote)` and
/// `Prompt`/`Agent`'s own list-vs-run action — both are resolved here so
/// every other call site only ever sees the already-sorted
/// `SyncCommand`/`AsyncCommand` shape.
pub(crate) fn classify(command: Option<Command>) -> Dispatch {
    match command {
        Some(Command::Lint(args)) => Dispatch::Sync(SyncCommand::Lint(args)),
        Some(Command::Models(args)) => {
            if args.remote {
                Dispatch::Async(AsyncCommand::ModelsRemote(args))
            } else {
                Dispatch::Sync(SyncCommand::ModelsLocal(args))
            }
        }
        Some(Command::Completions(args)) => Dispatch::Sync(SyncCommand::Completions(args)),
        Some(Command::Man(args)) => Dispatch::Sync(SyncCommand::Man(args)),
        Some(Command::Init(args)) => Dispatch::Sync(SyncCommand::Init(args)),
        Some(Command::Sessions(command)) => Dispatch::Sync(SyncCommand::Sessions(command)),
        Some(Command::Chat(args)) => Dispatch::Async(AsyncCommand::Chat(args)),
        Some(Command::Prompt(PromptCommand {
            action: PromptAction::List,
        })) => Dispatch::Sync(SyncCommand::PromptList),
        Some(Command::Prompt(PromptCommand {
            action: PromptAction::Run(args),
        })) => Dispatch::Async(AsyncCommand::PromptRun(args)),
        Some(Command::History(args)) => Dispatch::Sync(SyncCommand::History(args)),
        Some(Command::Graph(args)) => Dispatch::Sync(SyncCommand::Graph(args)),
        Some(Command::Workflow(WorkflowCommand {
            action: WorkflowAction::List,
        })) => Dispatch::Sync(SyncCommand::WorkflowList),
        Some(Command::Skill(SkillCommand {
            action: SkillAction::List,
        })) => Dispatch::Sync(SyncCommand::SkillList),
        Some(Command::Runs(command)) => Dispatch::Sync(SyncCommand::Runs(command)),
        Some(Command::Cache(command)) => Dispatch::Sync(SyncCommand::Cache(command)),
        Some(Command::Schema(args)) => Dispatch::Sync(SyncCommand::Schema(args)),
        Some(Command::Agent(AgentCommand {
            action: AgentAction::List,
        })) => Dispatch::Sync(SyncCommand::AgentList),
        Some(Command::Agent(AgentCommand {
            action: AgentAction::Run(args),
        })) => Dispatch::Async(AsyncCommand::AgentRun(args)),
        Some(Command::Run(args)) => Dispatch::Async(AsyncCommand::Run(args)),
        Some(Command::Doctor(args)) => Dispatch::Async(AsyncCommand::Doctor(args)),
        Some(Command::Compare(args)) => Dispatch::Async(AsyncCommand::Compare(args)),
        Some(Command::Test(args)) => Dispatch::Async(AsyncCommand::Test(args)),
        Some(Command::Eval(args)) => Dispatch::Async(AsyncCommand::Eval(args)),
        None => Dispatch::Async(AsyncCommand::Bare),
    }
}

/// Runs the `AsyncCommand` [`classify`] sorted onto the async path, on an
/// already-running Tokio runtime. `bare_chat` is only read for
/// `AsyncCommand::Bare` — see that variant's doc comment for why it isn't
/// part of `AsyncCommand` itself.
pub(crate) async fn run(
    command: AsyncCommand,
    bare_chat: ChatArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    match command {
        AsyncCommand::Run(run_args) => {
            run_workflow(
                run_args,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
        AsyncCommand::AgentRun(args) => {
            run_agent(args, config_source, cache_override, approve_tools, cancel).await
        }
        AsyncCommand::ModelsRemote(models_args) => {
            crate::models::run(models_args, config_source, Some(cancel)).await
        }
        AsyncCommand::Chat(chat_repl_args) => {
            repl::run(
                chat_repl_args,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
        AsyncCommand::PromptRun(run_args) => {
            run_prompt(
                run_args,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
        AsyncCommand::Doctor(doctor_args) => {
            doctor::run(doctor_args, config_source, Some(cancel)).await
        }
        AsyncCommand::Compare(compare_args) => {
            crate::compare::run(compare_args, config_source, cache_override, cancel).await
        }
        AsyncCommand::Test(test_args) => test_run::run(test_args, config_source, cancel).await,
        AsyncCommand::Eval(eval_args) => crate::eval::run(eval_args, config_source, cancel).await,
        AsyncCommand::Bare => {
            run_chat_or_repl(
                bare_chat,
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
pub(crate) fn cache_override(cache: bool, no_cache: bool) -> Option<bool> {
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

/// Runs the `SyncCommand` [`classify`] sorted onto the sync path — no Tokio
/// runtime behind this call at all (see this module's doc comment). Every
/// arm is reachable and exhaustive over `SyncCommand`'s own variants; there
/// is no catch-all/`bail!` arm because there is no `Command` variant left
/// for one to catch — `classify` already routed every async variant to
/// `AsyncCommand` instead.
pub(crate) fn run_blocking(command: SyncCommand, config_source: ConfigSource) -> Result<()> {
    match command {
        SyncCommand::Lint(lint_args) => lint::run(lint_args, config_source),
        SyncCommand::ModelsLocal(models_args) => {
            crate::models::run_local(models_args, config_source)
        }
        SyncCommand::Completions(completions_args) => {
            docgen::generate_completions(completions_args);
            Ok(())
        }
        SyncCommand::Man(man_args) => docgen::generate_man_pages(man_args),
        SyncCommand::Init(init_args) => crate::init::run(init_args),
        SyncCommand::Sessions(sessions_command) => crate::session::run(sessions_command),
        SyncCommand::PromptList => crate::prompt::list(&config::load_config(&config_source)?),
        SyncCommand::History(history_args) => history::run(history_args),
        SyncCommand::Graph(graph_args) => run_graph(graph_args),
        SyncCommand::AgentList => subagent::list(&config::load_config(&config_source)?),
        SyncCommand::WorkflowList => workflow::list(&config::load_config(&config_source)?),
        SyncCommand::SkillList => skill::list(&config::load_config(&config_source)?),
        SyncCommand::Runs(runs_command) => checkpoint::run(runs_command),
        SyncCommand::Cache(cache_command) => crate::cache::run(cache_command),
        SyncCommand::Schema(schema_args) => crate::schema::run(schema_args),
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

#[cfg(test)]
mod tests {
    use super::{Dispatch, classify};
    use crate::cli::Cli;

    /// Which of `run`/`run_blocking` a `classify` result would route to —
    /// the table below only needs this, not every field of the resulting
    /// `SyncCommand`/`AsyncCommand` payload.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Lane {
        Sync,
        Async,
    }

    fn lane(args: &[&str]) -> Lane {
        let cli = Cli::try_parse_from(args)
            .unwrap_or_else(|error| panic!("failed to parse {args:?}: {error}"));
        match classify(cli.command) {
            Dispatch::Sync(_) => Lane::Sync,
            Dispatch::Async(_) => Lane::Async,
        }
    }

    /// The table-driven test the design plan's B3 requires before merging:
    /// clap's derive doesn't enumerate `Command`'s variants for us, so this
    /// table is the only thing that actually exercises every one of them
    /// (`tests/man.rs` only covers 8 of ~23 — see its own comment). Every
    /// top-level subcommand appears at least once, `models`/`prompt`/`agent`
    /// each appear in both of their lanes (the three variants `classify`
    /// resolves by a field rather than by `Command` variant alone), and the
    /// bare invocation appears both plain and with a `Cli`-level global flag
    /// set — the row that would silently drop `--cache`/`--approve-tools`
    /// if `classify` were ever changed to consume the whole `Cli` instead of
    /// just `cli.command` (see this module's doc comment on why it doesn't).
    #[test]
    fn every_subcommand_classifies_to_the_expected_lane() {
        let cases: &[(&[&str], Lane)] = &[
            (&["lait", "run", "workflow.yml", "hi"], Lane::Async),
            (&["lait", "agent", "run", "agent.md", "hi"], Lane::Async),
            (&["lait", "agent", "list"], Lane::Sync),
            (&["lait", "lint", "workflow.yml"], Lane::Sync),
            (&["lait", "models"], Lane::Sync),
            (&["lait", "models", "--remote"], Lane::Async),
            (&["lait", "completions", "bash"], Lane::Sync),
            (&["lait", "man"], Lane::Sync),
            (&["lait", "init"], Lane::Sync),
            (&["lait", "sessions", "list"], Lane::Sync),
            (&["lait", "chat"], Lane::Async),
            (&["lait", "prompt", "list"], Lane::Sync),
            (&["lait", "prompt", "run", "name", "hi"], Lane::Async),
            (&["lait", "history"], Lane::Sync),
            (&["lait", "graph", "workflow.yml"], Lane::Sync),
            (&["lait", "workflow", "list"], Lane::Sync),
            (&["lait", "skill", "list"], Lane::Sync),
            (&["lait", "runs", "list"], Lane::Sync),
            (&["lait", "cache", "clear"], Lane::Sync),
            (&["lait", "schema", "workflow"], Lane::Sync),
            (&["lait", "doctor"], Lane::Async),
            (
                &["lait", "compare", "--model", "a", "--model", "b", "hi"],
                Lane::Async,
            ),
            (&["lait", "test", "case.yml"], Lane::Async),
            (&["lait", "eval", "eval.yml"], Lane::Async),
            (&["lait", "hi"], Lane::Async),
            (&["lait"], Lane::Async),
            (&["lait", "--cache", "hi"], Lane::Async),
            (&["lait", "--approve-tools", "hi"], Lane::Async),
        ];
        for (args, expected) in cases {
            assert_eq!(lane(args), *expected, "args: {args:?}");
        }
    }
}
