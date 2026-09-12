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
use async_openai::types::chat::{ChatCompletionRequestMessage, ResponseFormat};

use crate::{
    agent, attachment, chat, checkpoint,
    cli::{
        AgentAction, AgentCommand, AgentRunArgs, CacheCommand, ChatArgs, ChatReplArgs, Command,
        CompareArgs, CompletionsArgs, DoctorArgs, EvalArgs, GraphArgs, GraphFormat, HistoryArgs,
        InitArgs, LintArgs, ManArgs, ModelsArgs, OutputArgs, PromptAction, PromptCommand,
        PromptRunArgs, ReportingArgs, RunArgs, RunsCommand, SchemaArgs, SessionsCommand,
        SkillAction, SkillCommand, TestArgs, WorkflowAction, WorkflowCommand,
    },
    config::{self, ConfigSource, ModelMap},
    docgen, doctor,
    engine::{
        AgentTurn, AppServices, CapabilityOverrides, EndpointOverrides, PromptTurn,
        RequestSettings, RunContext, SamplingOverrides, agent_file_settings, call_agent,
        resolve_request_settings,
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
    // Boxed: `AsyncCommand` (264 bytes, dominated by its largest payload
    // variant) would otherwise make every `Dispatch` that large even along
    // the `Sync` arm (56 bytes) — `clippy::large_enum_variant`. The extra
    // indirection is paid once per invocation, not on a hot path.
    Async(Box<AsyncCommand>),
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
                Dispatch::Async(Box::new(AsyncCommand::ModelsRemote(args)))
            } else {
                Dispatch::Sync(SyncCommand::ModelsLocal(args))
            }
        }
        Some(Command::Completions(args)) => Dispatch::Sync(SyncCommand::Completions(args)),
        Some(Command::Man(args)) => Dispatch::Sync(SyncCommand::Man(args)),
        Some(Command::Init(args)) => Dispatch::Sync(SyncCommand::Init(args)),
        Some(Command::Sessions(command)) => Dispatch::Sync(SyncCommand::Sessions(command)),
        Some(Command::Chat(args)) => Dispatch::Async(Box::new(AsyncCommand::Chat(args))),
        Some(Command::Prompt(PromptCommand {
            action: PromptAction::List,
        })) => Dispatch::Sync(SyncCommand::PromptList),
        Some(Command::Prompt(PromptCommand {
            action: PromptAction::Run(args),
        })) => Dispatch::Async(Box::new(AsyncCommand::PromptRun(args))),
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
        })) => Dispatch::Async(Box::new(AsyncCommand::AgentRun(args))),
        Some(Command::Run(args)) => Dispatch::Async(Box::new(AsyncCommand::Run(args))),
        Some(Command::Doctor(args)) => Dispatch::Async(Box::new(AsyncCommand::Doctor(args))),
        Some(Command::Compare(args)) => Dispatch::Async(Box::new(AsyncCommand::Compare(args))),
        Some(Command::Test(args)) => Dispatch::Async(Box::new(AsyncCommand::Test(args))),
        Some(Command::Eval(args)) => Dispatch::Async(Box::new(AsyncCommand::Eval(args))),
        None => Dispatch::Async(Box::new(AsyncCommand::Bare)),
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

/// Builds the `AppServices`/`RunContext` pair `run_chat`/`run_prompt`/
/// `run_agent` each need once `file_config` is loaded: resolve the cache
/// policy, construct the services, and apply cache/approve-tools to a fresh
/// `RunContext`. All three used to repeat this identical five-line block by
/// hand; factoring it out means a future policy added here (or a bug fixed
/// in it) can't drift between the three.
fn build_run_context(
    file_config: &Arc<config::ConfigFile>,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> (Arc<AppServices>, RunContext) {
    let (cache_enabled, cache_ttl) = chat::resolve_cache_settings(cache_override, file_config);
    let services = Arc::new(AppServices::new(Arc::clone(file_config)));
    let env = RunContext::new(Arc::clone(&services), cancel)
        .with_cache(cache_enabled, cache_ttl)
        .with_approve_tools(approve_tools);
    (services, env)
}

/// The reporting-related pieces `finish_prompt_or_agent_run` needs beyond
/// the run's own content, bundled into one parameter (like `ProcessTasks`
/// in `process.rs`) so adding them alongside `kind`/`output`/`model_id`/
/// `prompt` doesn't trip `clippy::too_many_arguments`.
struct RunReport<'a> {
    output_args: &'a OutputArgs,
    reporting: &'a ReportingArgs,
    file_config: &'a config::ConfigFile,
    usage: &'a usage::UsageTally,
}

/// Emits and records a `run_prompt`/`run_agent` result: prints `output`
/// (respecting `--output`/`-o`), then records the run to history and prints
/// the usage summary when asked. The two callers differ only in `kind`
/// (`"prompt"`/`"agent"`), which model id to attribute the run to, and which
/// text counts as the "prompt" in history — everything else here used to be
/// copied verbatim between them.
fn finish_prompt_or_agent_run(
    kind: &'static str,
    output: &str,
    model_id: &str,
    prompt: &str,
    report: RunReport<'_>,
) -> Result<()> {
    report::emit_run_output(
        output,
        report.usage.total(),
        report.output_args,
        report.file_config,
    )?;
    report::finish_run(
        report::RunRecord {
            kind,
            model: Some(model_id),
            prompt,
            response: output,
        },
        report.reporting.no_history,
        report.file_config,
        report.usage,
        report.reporting.show_usage,
    )
}

/// The tail shared by both of `run_chat`'s branches: records the turn to
/// session/history and prints the usage summary when asked. `content` is
/// the streamed outcome's accumulated text or the non-streamed response's
/// rendered content, whichever branch reached here — the streamed branch
/// still records the response's usage into `env.usage` itself first (see
/// its own comment on why that can't move here).
fn finish_chat_run(
    chat: &ChatArgs,
    file_config: &config::ConfigFile,
    model_id: &str,
    prompt: &str,
    content: &str,
    env: &RunContext,
    show_usage: bool,
) -> Result<()> {
    chat::finish_chat_turn(
        chat.shared.session.as_deref(),
        chat.shared.reporting.no_history,
        file_config,
        model_id,
        prompt,
        content,
        env.usage.total(),
    )?;
    if show_usage {
        usage::print_usage_summary(&env.usage);
    }
    Ok(())
}

/// Runs a single-shot chat request with an already-resolved `prompt` — see
/// `run_chat_or_repl`, the only caller, for how `prompt` was resolved (a
/// CLI argument and/or piped stdin).
/// Display/output policy derived from `--quiet`/`--show-reasoning`/
/// `--show-usage`/`--render`/`-o`, shared by [`run_chat`]'s streaming and
/// non-streaming completion paths.
struct ChatDisplayPolicy<'a> {
    show_reasoning: bool,
    show_usage: bool,
    render_enabled: bool,
    /// `-o -` is an explicit "stdout", the same as no `-o` at all.
    output_path: Option<&'a std::path::Path>,
}

impl<'a> ChatDisplayPolicy<'a> {
    fn resolve(chat: &'a ChatArgs, file_config: &config::ConfigFile) -> Self {
        // `--quiet` keeps the response body and drops every note around it.
        Self {
            show_reasoning: chat.shared.show_reasoning && !chat.quiet,
            show_usage: chat.shared.reporting.show_usage && !chat.quiet,
            render_enabled: chat.output.render || file_config.default.render.unwrap_or(false),
            output_path: chat
                .output
                .output
                .as_deref()
                .filter(|path| path.as_os_str() != "-"),
        }
    }
}

/// Everything [`run_chat`] needs to send its one completion request, built
/// by [`prepare_chat_request`]: config, resolved model/sampling settings,
/// the fully assembled prompt (template-rendered and file-attachment-
/// appended), and the run's services/context/display policy.
struct ChatRequest<'a> {
    file_config: Arc<config::ConfigFile>,
    settings: RequestSettings,
    response_format: Option<ResponseFormat>,
    prompt: String,
    system_prompt: Option<String>,
    session_history: Vec<ChatCompletionRequestMessage>,
    image_urls: Vec<String>,
    services: Arc<AppServices>,
    env: RunContext,
    display: ChatDisplayPolicy<'a>,
}

/// Resolves everything a chat completion request needs, ahead of actually
/// sending it: `-p`/`--prompt-name` template rendering, model/sampling
/// settings, the JSON Schema `--json-schema` requests, file attachments/
/// system prompt/image URLs (read concurrently — see the `tokio::try_join!`
/// below), session history, and the run's services/context.
async fn prepare_chat_request<'a>(
    chat: &'a ChatArgs,
    prompt: String,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<ChatRequest<'a>> {
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

    let settings =
        chat::resolve_chat_settings(&chat.shared, prompt_model_fallback.as_deref(), &file_config)?;

    let response_format = chat
        .json_schema
        .as_deref()
        .map(|path| schema::load_json_schema(path, &chat.schema_name))
        .transpose()?;

    // These three reads are independent of each other and of everything
    // above: file attachments only need `chat.files`, the system prompt
    // only needs `chat.shared`/`file_config`, and image URLs only need
    // `chat.images`. Running them concurrently rather than one after
    // another (as `workflow/exec.rs` already does for its own file+image
    // pair via `tokio::try_join!`) shortens the wall-clock delay before the
    // first token on e.g. `lait -f a.rs -f b.rs --image x.png "..."`.
    let (file_context, system_prompt, image_urls) = tokio::try_join!(
        attachment::read_file_attachments(&chat.files),
        chat::resolve_system_prompt(&chat.shared, &file_config, Some(cancel.clone())),
        attachment::resolve_image_urls(&chat.images),
    )?;
    let prompt = match file_context {
        Some(file_context) => format!("{prompt}\n\n{file_context}"),
        None => prompt,
    };
    let session_history = chat::load_session_history(chat.shared.session.as_deref())?;
    let (services, env) = build_run_context(&file_config, cache_override, approve_tools, cancel);
    let display = ChatDisplayPolicy::resolve(chat, &file_config);

    Ok(ChatRequest {
        file_config,
        settings,
        response_format,
        prompt,
        system_prompt,
        session_history,
        image_urls,
        services,
        env,
        display,
    })
}

async fn run_chat(
    chat: ChatArgs,
    prompt: String,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let ChatRequest {
        file_config,
        settings,
        response_format,
        prompt,
        system_prompt,
        session_history,
        image_urls,
        services,
        env,
        display,
    } = prepare_chat_request(
        &chat,
        prompt,
        config_source,
        cache_override,
        approve_tools,
        cancel,
    )
    .await?;

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
                display.show_usage,
                display.show_reasoning,
                display.output_path,
                Some(env.operation_token()),
            ))
            .await?;
        // Streamed usage arrives on the final chunk rather than through
        // `complete`; feed it into the same tally so both chat paths share
        // one summary format and so `env.usage.total()` below reflects it.
        if let Some(usage) = outcome.usage {
            env.usage.record(&settings.usage_label, usage);
        }
        return finish_chat_run(
            &chat,
            &file_config,
            &settings.resolved_model.model_id,
            &prompt,
            &outcome.content,
            &env,
            display.show_usage,
        );
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

    match display.output_path {
        Some(path) => {
            // The file gets the body alone; reasoning, when requested,
            // becomes a stderr note like usage.
            if display.show_reasoning
                && let Some(reasoning) = response::response_reasoning(&response)
            {
                eprintln!("Reasoning:\n{reasoning}\n");
            }
            let body = response::render_response(
                &response,
                response::RenderOptions {
                    as_json: chat.output.json,
                    show_reasoning: false,
                },
            )?;
            report::emit_output(&body, Some(path), false)?;
        }
        None => {
            let output = response::render_response(
                &response,
                response::RenderOptions {
                    as_json: chat.output.json,
                    show_reasoning: display.show_reasoning,
                },
            )?;
            // `--json`'s output is machine-readable and never rendered as
            // Markdown; `chat.stream`'s branch above already returned before
            // reaching here, so `--render` never has to reckon with a
            // partial streamed response either — see `report::maybe_render`.
            report::emit_output(&output, None, !chat.output.json && display.render_enabled)?;
        }
    }
    let content = response::content_text(&response);
    finish_chat_run(
        &chat,
        &file_config,
        &settings.resolved_model.model_id,
        &prompt,
        content,
        &env,
        display.show_usage,
    )
}

/// Shared by [`run_prompt`]/[`run_agent`]: both resolve `INPUT` the same way
/// (`chat::resolve_input_with_stdin_cancellable`) and fail identically when
/// neither a positional argument nor piped stdin supplied one.
fn missing_input_error() -> anyhow::Error {
    anyhow!("an INPUT is required; provide one or pipe input via stdin")
}

/// Shared by `workflow_run::run_workflow` and `compare::run`: both resolve
/// `PROMPT` the same way (`chat::resolve_input_with_stdin_cancellable`) and
/// fail identically when neither a positional argument nor piped stdin
/// supplied one. `pub(crate)` (unlike [`missing_input_error`]) because
/// `compare` is a sibling module of `app`, not a descendant.
pub(crate) fn missing_prompt_error() -> anyhow::Error {
    anyhow!("a PROMPT is required; provide one or pipe input via stdin")
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
            .ok_or_else(missing_input_error)?;
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
        EndpointOverrides::default(),
        CapabilityOverrides::default(),
        &ModelMap::default(),
        &file_config,
    )?
    .with_usage_label(format!("prompt '{}'", args.name));

    let (services, env) = build_run_context(&file_config, cache_override, approve_tools, cancel);
    let response = services
        .finish(settings.complete(
            &env,
            &[],
            PromptTurn::simple(None, &prompt_text),
            None,
            Some(env.operation_token()),
        ))
        .await?;
    let output = response::render_response(
        &response,
        response::RenderOptions {
            as_json: false,
            show_reasoning: false,
        },
    )?;
    finish_prompt_or_agent_run(
        "prompt",
        &output,
        &settings.resolved_model.model_id,
        &prompt_text,
        RunReport {
            output_args: &args.output,
            reporting: &args.reporting,
            file_config: &file_config,
            usage: &env.usage,
        },
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
            .ok_or_else(missing_input_error)?;
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

    let (services, env) = build_run_context(&file_config, cache_override, approve_tools, cancel);
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
    finish_prompt_or_agent_run(
        "agent",
        &output,
        &settings.resolved_model.model_id,
        &raw_input,
        RunReport {
            output_args: &args.output,
            reporting: &args.reporting,
            file_config: &file_config,
            usage: &env.usage,
        },
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
