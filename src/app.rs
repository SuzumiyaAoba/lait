//! Top-level command dispatch: turns a parsed `cli::Command` into a call
//! into the module that actually implements it (`lint::run`,
//! `workflow_run::run_workflow`, `repl::run`, `chat_run::run_chat_or_repl`,
//! `prompt_run::run_prompt`/`run_agent`, ...). This module should stay pure
//! dispatch — building a chat turn's settings/history/cache policy lives in
//! `chat` instead (see `chat.rs`'s own doc comment for why that split
//! exists), and [`run`]/[`run_blocking`] below only wire pieces together for
//! their one entry point each, never implementing a command's body directly.
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

use anyhow::{Context, Result};

use crate::{
    chat, checkpoint,
    cli::{
        AgentAction, AgentCommand, AgentRunArgs, CacheCommand, ChatArgs, ChatReplArgs, Command,
        CompareArgs, CompletionsArgs, DecideArgs, DepsAction, DepsAddArgs, DepsCommand,
        DepsInstallArgs, DepsNameArgs, DepsUpdateArgs, DoctorArgs, EvalArgs, GraphArgs,
        GraphFormat, HistoryArgs, InitArgs, LintArgs, ManArgs, ModelsArgs, PromptAction,
        PromptCommand, PromptRunArgs, RunArgs, RunsCommand, SchemaArgs, ServeArgs, SessionsCommand,
        SkillAction, SkillCommand, TestArgs, TraceAction, TraceCommand, TraceShowArgs,
        WorkflowAction, WorkflowCommand,
    },
    config::{self, ConfigSource},
    deps, docgen,
    engine::{AppServices, RunContext},
    history, lint, skill, subagent, trace, workflow,
};

mod chat_run;
mod compare;
mod decide;
mod doctor;
mod eval;
mod models;
mod prompt_run;
mod repl;
mod serve;
mod test_run;
mod workflow_run;

use chat_run::run_chat_or_repl;
use prompt_run::{run_agent, run_prompt};
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
    TraceShow(TraceShowArgs),
    DepsRemove(DepsNameArgs),
    DepsList,
    DepsVerify,
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
    Serve(ServeArgs),
    Decide(DecideArgs),
    /// The `lait deps` actions that reach GitHub — `remove`/`list`/`verify`
    /// are the [`SyncCommand`] half, being manifest/lock/disk-only.
    DepsAdd(DepsAddArgs),
    DepsInstall(DepsInstallArgs),
    DepsUpdate(DepsUpdateArgs),
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
        Some(Command::Serve(args)) => Dispatch::Async(Box::new(AsyncCommand::Serve(args))),
        Some(Command::Decide(args)) => Dispatch::Async(Box::new(AsyncCommand::Decide(args))),
        // `deps` classifies by action like `prompt`/`agent` do: the three
        // fetch-bound actions are async, the manifest/lock/disk-only ones
        // stay off the Tokio runtime entirely.
        Some(Command::Deps(DepsCommand { action })) => match action {
            DepsAction::Add(args) => Dispatch::Async(Box::new(AsyncCommand::DepsAdd(args))),
            DepsAction::Install(args) => Dispatch::Async(Box::new(AsyncCommand::DepsInstall(args))),
            DepsAction::Update(args) => Dispatch::Async(Box::new(AsyncCommand::DepsUpdate(args))),
            DepsAction::Remove(args) => Dispatch::Sync(SyncCommand::DepsRemove(args)),
            DepsAction::List => Dispatch::Sync(SyncCommand::DepsList),
            DepsAction::Verify => Dispatch::Sync(SyncCommand::DepsVerify),
        },
        Some(Command::Trace(TraceCommand {
            action: TraceAction::Show(args),
        })) => Dispatch::Sync(SyncCommand::TraceShow(args)),
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
            models::run(models_args, config_source, cancel).await
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
        AsyncCommand::Doctor(doctor_args) => doctor::run(doctor_args, config_source, cancel).await,
        AsyncCommand::Compare(compare_args) => {
            compare::run(compare_args, config_source, cache_override, cancel).await
        }
        AsyncCommand::Test(test_args) => test_run::run(test_args, config_source, cancel).await,
        AsyncCommand::Eval(eval_args) => eval::run(eval_args, config_source, cancel).await,
        AsyncCommand::Serve(serve_args) => serve::run(serve_args, config_source, cancel).await,
        AsyncCommand::Decide(decide_args) => decide::run(decide_args, config_source, cancel).await,
        // Deps commands take no `config_source`: they operate on
        // `lait.deps.yml`/`lait.lock`, which are discovered from the current
        // directory independently of the config search (see `deps::manifest`
        // for the shared upward-walk rule).
        AsyncCommand::DepsAdd(args) => deps::run_add(args, cancel).await,
        AsyncCommand::DepsInstall(args) => deps::run_install(args, cancel).await,
        AsyncCommand::DepsUpdate(args) => deps::run_update(args, cancel).await,
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

/// Runs the `SyncCommand` [`classify`] sorted onto the sync path — no Tokio
/// runtime behind this call at all (see this module's doc comment). Every
/// arm is reachable and exhaustive over `SyncCommand`'s own variants; there
/// is no catch-all/`bail!` arm because there is no `Command` variant left
/// for one to catch — `classify` already routed every async variant to
/// `AsyncCommand` instead.
pub(crate) fn run_blocking(command: SyncCommand, config_source: ConfigSource) -> Result<()> {
    match command {
        SyncCommand::Lint(lint_args) => lint::run(lint_args, config_source),
        SyncCommand::ModelsLocal(models_args) => models::run_local(models_args, config_source),
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
        SyncCommand::TraceShow(trace_show_args) => run_trace_show(trace_show_args),
        SyncCommand::DepsRemove(args) => deps::run_remove(args),
        SyncCommand::DepsList => deps::run_list(),
        SyncCommand::DepsVerify => deps::run_verify(),
    }
}

/// Runs `lait trace show`: reads a `--trace-file`-written JSONL log back and
/// prints it (a human-readable, one-line-per-event list; `--json` for a
/// parsed array instead), sorted the same way `TraceCollector::events`
/// already orders them. Pure local work, like `run_graph` below — no
/// `lait.config.yml`, no model resolution.
fn run_trace_show(args: TraceShowArgs) -> Result<()> {
    let contents = std::fs::read_to_string(&args.file)
        .with_context(|| format!("failed to read trace file '{}'", args.file.display()))?;
    let mut events = Vec::new();
    for (line_number, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: trace::TraceEvent = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse trace event on line {} of '{}'",
                line_number + 1,
                args.file.display(),
            )
        })?;
        events.push(event);
    }
    events.sort_by_key(|event| event.seq);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&events)
                .context("failed to render trace events as JSON")?
        );
        return Ok(());
    }
    if events.is_empty() {
        println!("(no events)");
        return Ok(());
    }
    for event in &events {
        let attributes = event
            .attributes
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "[{:>6}ms] {:<12} {} — {}{}",
            event.duration_ms,
            event.operation,
            event.label,
            event.start.format("%H:%M:%S%.3f"),
            if attributes.is_empty() {
                String::new()
            } else {
                format!("  ({attributes})")
            },
        );
    }
    Ok(())
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

/// Upper bound on concurrently in-flight per-case runs for `lait test` and
/// `lait eval` — the same value both commands spelled out as their own
/// `TEST_CONCURRENCY`/`EVAL_CONCURRENCY` constant. Matches
/// `engine::tool_loop::MAX_CONCURRENT_TOOL_CALLS`'s rationale: independent
/// runs, bounded so a large suite doesn't open unbounded concurrent
/// connections to a replay directory or configured endpoint.
pub(super) const SUITE_CONCURRENCY: usize = 8;

/// Arms the process-wide Ctrl-C handler for `cancel`, then loads
/// `lait.config.yml` — the two-step prefix every single-shot async command
/// used to spell out by hand. The handler goes first so a Ctrl-C landing
/// during the config read itself is a typed cancellation, not the default
/// terminate. Callers whose ordering must differ keep the pieces separate:
/// `test_run` expands its target list first so a bad `--paths` errors
/// before a bad config, `doctor` reads the config non-fatally through its
/// own `check_config_load`, and `run_agent` folds the load into a
/// `try_join!` with its other initial reads.
pub(super) async fn load_config(
    config_source: &ConfigSource,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Arc<config::ConfigFile>> {
    crate::signal::spawn_handler(cancel.clone());
    Ok(Arc::new(
        config::load_config_cancellable(config_source, cancel.clone()).await?,
    ))
}

/// Builds the `AppServices`/`RunContext` pair `run_chat`/`run_prompt`/
/// `run_agent` each need once `file_config` is loaded: resolve the cache
/// policy, construct the services, and apply cache/approve-tools to a fresh
/// `RunContext`. All three used to repeat this identical five-line block by
/// hand; factoring it out means a future policy added here (or a bug fixed
/// in it) can't drift between the three.
pub(super) fn build_run_context(
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

    /// The table-driven test that guards `classify`'s exhaustiveness: clap's
    /// derive doesn't enumerate `Command`'s variants for us, so this table is
    /// the only thing that actually exercises every one of them
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
            (&["lait", "trace", "show", "trace.jsonl"], Lane::Sync),
            (
                &["lait", "deps", "add", "owner/repo/workflow.yml"],
                Lane::Async,
            ),
            (&["lait", "deps", "install"], Lane::Async),
            (&["lait", "deps", "update"], Lane::Async),
            (&["lait", "deps", "remove", "name"], Lane::Sync),
            (&["lait", "deps", "list"], Lane::Sync),
            (&["lait", "deps", "verify"], Lane::Sync),
            (&["lait", "serve", "--mcp"], Lane::Async),
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
