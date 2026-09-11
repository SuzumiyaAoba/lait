//! The `clap`-derived command-line grammar: `Cli`, its `Command` subcommand
//! enum (one variant per top-level subcommand, `lait chat`/`lait run`/...),
//! and each subcommand's own `*Args` struct. Deliberately kept as one file
//! rather than split by command group (see the design plan's rejection of a
//! `cli/` split) — the bulk of its line count is `#[derive]`/`#[arg]`
//! attributes and field doc comments (`--help` text), not logic, so grouping
//! by subcommand would move declarations around without reducing what a
//! reader has to hold in mind.
//!
//! `app::classify`/`app::run_blocking`/`app::run` are the consumers that
//! turn a parsed `Command` into behavior; this module owns only the shape of
//! the arguments, not their dispatch.

use std::path::PathBuf;

use crate::reasoning::ReasoningEffort;
use clap::{
    ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    error::ErrorKind, parser::ValueSource,
};

/// Lightweight AI Tool command-line interface.
#[derive(Debug, Parser)]
#[command(name = "lait", version, about = "Lightweight AI Tool")]
#[command(subcommand_precedence_over_arg = true)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    #[command(flatten)]
    pub(crate) chat: ChatArgs,

    /// Do not read lait.config.yml from the current directory or any of its
    /// ancestors.
    #[arg(long, global = true, conflicts_with = "config")]
    pub(crate) no_config: bool,

    /// Read configuration from PATH instead of searching for
    /// lait.config.yml starting at the current directory and walking up
    /// through its ancestors (like git looks for `.git`). Unlike the
    /// default search, a missing PATH is an error.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,

    /// Do not read a `.env` file from the current directory. (Acted on
    /// before argument parsing — see `main` — so this declaration only
    /// provides `--help` output and validation.)
    #[arg(long, global = true)]
    pub(crate) no_env: bool,

    /// Increase log verbosity: once (`-v`) for debug-level tracing of
    /// resolved request settings, workflow step timing/retries, and tool
    /// calls; twice (`-vv`) to also dump full request/response JSON. Always
    /// written to stderr, never stdout, so a piped answer stays clean; API
    /// key values are omitted; only their source is logged. `LAIT_LOG` (an `EnvFilter` directive string, e.g.
    /// `debug` or `lait=trace,reqwest=info`) overrides this when set. See
    /// `crate::logging`.
    #[arg(short = 'v', long = "verbose", global = true, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,

    /// Cache completion responses under `.lait/cache/`, keyed by the
    /// request's base URL/model/sampling/messages/tools/response format (not
    /// the API key). A hit skips the network call entirely and prints a
    /// `note:` to stderr. Falls back to `default.cache` in lait.config.yml
    /// when neither this nor `--no-cache` is passed; off by default.
    /// Streamed (`--stream`) responses are never cached. See
    /// `crate::cache`.
    #[arg(long, global = true, conflicts_with = "no_cache")]
    pub(crate) cache: bool,

    /// Never use or write the response disk cache, overriding
    /// `default.cache` in lait.config.yml.
    #[arg(long, global = true)]
    pub(crate) no_cache: bool,

    /// Before running an MCP/subagent/`tools:` shell tool the model calls,
    /// print its name and arguments to stderr (plus the actual command a
    /// shell tool would exec, once its `command:` template is rendered) and
    /// ask on stdin whether to allow it: `y` (this call only), `n` (deny
    /// this call — the model sees a denial as the tool's result and the run
    /// continues), or `a` (allow this tool name for the rest of the run
    /// without asking again). Every call in one round is confirmed before
    /// any of them run, one at a time, so prompts never interleave. A denial
    /// from this or from `tool_policy` in lait.config.yml (see
    /// `crate::config::ToolPolicy`) still lets the tool loop continue — only
    /// an actual error, or the model giving up, ends the run. Requires an
    /// interactive stdin; errors immediately otherwise (piped/CI input has
    /// no one to answer, and there is no way to tell a closed pipe from a
    /// slow human — see `workflow::ask::run_ask`'s same reasoning).
    #[arg(long, global = true)]
    pub(crate) approve_tools: bool,
}

impl Cli {
    /// Parse the command line while allowing global options on either side of
    /// a subcommand. The root-level chat arguments remain exclusive to bare
    /// chat mode; clap's command-wide `args_conflicts_with_subcommands` cannot
    /// express that distinction because it also treats global options as
    /// conflicting arguments.
    pub(crate) fn parse() -> Self {
        match Self::try_parse() {
            Ok(cli) => cli,
            Err(error) => error.exit(),
        }
    }

    /// Parse the process arguments without exiting on an error.
    fn try_parse() -> Result<Self, clap::Error> {
        Self::try_parse_from(std::env::args_os())
    }

    /// Parse an argument iterator without exiting on an error.
    pub(crate) fn try_parse_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let mut command = <Self as CommandFactory>::command();
        let mut matches = command.try_get_matches_from_mut(itr)?;
        Self::from_matches(&mut command, &mut matches)
    }

    fn from_matches(
        command: &mut clap::Command,
        matches: &mut ArgMatches,
    ) -> Result<Self, clap::Error> {
        let subcommand = matches.subcommand_name().map(str::to_owned);
        let chat_argument = command
            .get_arguments()
            .filter(|argument| !argument.is_global_set())
            .find(|argument| {
                matches.value_source(argument.get_id().as_str()) == Some(ValueSource::CommandLine)
            })
            .map(|argument| argument.to_string());

        let cli = <Self as FromArgMatches>::from_arg_matches_mut(matches)
            .map_err(|error| error.format(command))?;

        if cli.command.is_some()
            && let Some(argument) = chat_argument
        {
            let subcommand = subcommand.as_deref().unwrap_or("<subcommand>");
            return Err(command.error(
                ErrorKind::ArgumentConflict,
                format!("the argument '{argument}' cannot be used with subcommand '{subcommand}'"),
            ));
        }

        Ok(cli)
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a YAML-defined workflow (see workflow.yml).
    Run(RunArgs),
    /// Work with agent Markdown files (frontmatter + system prompt template).
    Agent(AgentCommand),
    /// Statically check workflow (.yml/.yaml) and agent (.md) files for
    /// structural and reference errors, without running them.
    Lint(LintArgs),
    /// List the model aliases configured in lait.config.yml, or the models
    /// the server itself offers with `--remote`.
    Models(ModelsArgs),
    /// Generate a shell completion script on stdout (e.g. `lait completions
    /// zsh > ~/.zfunc/_lait`).
    Completions(CompletionsArgs),
    /// Generate roff man pages for lait and every subcommand (lait.1,
    /// lait-run.1, ...).
    Man(ManArgs),
    /// Generate starter files: `lait init` writes a minimal lait.config.yml,
    /// `lait init workflow [PATH]` / `lait init agent [PATH]` write commented
    /// workflow.yml / agent.md scaffolds.
    Init(InitArgs),
    /// List, inspect, or delete saved `--session` conversations.
    Sessions(SessionsCommand),
    /// Start an interactive, multi-turn chat REPL (see docs/usage/ja/chat.md).
    Chat(ChatReplArgs),
    /// Run a named prompt template from `prompts:` in lait.config.yml
    /// (`lait prompt run <NAME>`), or list every configured prompt
    /// (`lait prompt list`).
    Prompt(PromptCommand),
    /// List, show, or search recorded chat/agent/workflow/prompt runs (see
    /// docs/usage/ja/history.md).
    History(HistoryArgs),
    /// Print a workflow's control-flow structure as a Mermaid or DOT graph
    /// (see docs/usage/ja/workflow.md).
    Graph(GraphArgs),
    /// List the workflows registered under `workflows:` in lait.config.yml
    /// (`lait workflow list`), runnable by name via `lait run <NAME>`.
    Workflow(WorkflowCommand),
    /// List the skill files registered under `skills:` in lait.config.yml
    /// (`lait skill list`).
    Skill(SkillCommand),
    /// List or inspect checkpointed `lait run --checkpoint` runs
    /// (`.lait/runs/`), resumable with `lait run ... --resume <RUN_ID>`.
    Runs(RunsCommand),
    /// Manage the response disk cache (`.lait/cache/`, see `--cache`).
    Cache(CacheCommand),
    /// Print the JSON Schema (draft 2020-12) for workflow.yml, lait.config.yml,
    /// or an agent file's frontmatter, for editor completion/validation (e.g.
    /// yaml-language-server). See docs/usage/ja/schema.md.
    Schema(SchemaArgs),
    /// Diagnose the environment/configuration/connectivity in one pass:
    /// lait.config.yml parsing, `${VAR}` environment variables, model
    /// resolution, provider connectivity and authentication, whether
    /// configured model ids exist on the server, `mcp_servers:` startup, and
    /// `agents:`/`skills:` file references. See docs/usage/ja/troubleshooting.md.
    Doctor(DoctorArgs),
    /// Send the same prompt to two or more models and print each one's
    /// response, timing, and usage side by side. See docs/usage/ja/compare.md.
    Compare(CompareArgs),
    /// Run test definition files (YAML: a workflow, input/vars, a `--record`ed
    /// replay cassette directory, and assertions) with no network access,
    /// reporting pass/fail. See docs/usage/ja/testing.md.
    Test(TestArgs),
    /// Run an eval definition file (YAML: a target workflow or model+prompt,
    /// test cases, and `assert:` checks — including `llm_judge`) against a
    /// live model connection, reporting a per-case success rate. See
    /// docs/usage/ja/eval.md.
    Eval(EvalArgs),
}

#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// Print machine-readable JSON instead of a human-readable report.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SchemaArgs {
    /// Which file format to print the JSON Schema for.
    #[arg(value_enum, value_name = "KIND")]
    pub(crate) kind: SchemaKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum SchemaKind {
    /// The schema for a workflow YAML file (`workflow.yml`).
    Workflow,
    /// The schema for `lait.config.yml`.
    Config,
    /// The schema for an agent Markdown file's YAML frontmatter (`agent.md`).
    Agent,
}

#[derive(Debug, Args)]
pub(crate) struct CompareArgs {
    /// A configured model name or model identifier to compare. Repeatable;
    /// at least two are required (enforced by `app::run`, not clap, the same
    /// way `lait run`/`lait agent run` validate PROMPT/INPUT at the app
    /// layer rather than declaratively).
    #[arg(long = "model", value_name = "NAME", required = true)]
    pub(crate) models: Vec<String>,

    /// The prompt sent to every model. May be omitted when input is piped
    /// via stdin (which is then used as the prompt).
    #[arg(value_name = "PROMPT")]
    pub(crate) prompt: Option<String>,

    /// The reasoning effort to request from every model, overriding each
    /// model's own configured default.
    #[arg(long, value_enum)]
    pub(crate) reasoning_effort: Option<ReasoningEffort>,

    /// Sampling temperature (0.0-2.0), applied uniformly to every model
    /// compared, overriding each model's own configured default.
    #[arg(long)]
    pub(crate) temperature: Option<f64>,

    /// Nucleus sampling probability mass (0.0-1.0), applied uniformly to
    /// every model compared, overriding each model's own configured default.
    #[arg(long)]
    pub(crate) top_p: Option<f64>,

    /// An upper bound on generated tokens, applied uniformly to every model
    /// compared, overriding each model's own configured default.
    #[arg(long)]
    pub(crate) max_tokens: Option<u32>,

    /// Print machine-readable JSON (an array of per-model results) instead
    /// of a human-readable report.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct TestArgs {
    /// Test definition files (.yml/.yaml) and/or directories to search
    /// recursively for them (see docs/usage/ja/testing.md).
    #[arg(value_name = "FILE", required = true)]
    pub(crate) paths: Vec<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = TestFormat::Text)]
    pub(crate) format: TestFormat,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum TestFormat {
    /// A per-file pass/fail report with failing assertions detailed.
    #[default]
    Text,
    /// A structured JSON report (one entry per test file).
    Json,
}

#[derive(Debug, Args)]
pub(crate) struct EvalArgs {
    /// Path to the eval definition YAML file (see docs/usage/ja/eval.md).
    #[arg(value_name = "FILE")]
    pub(crate) file: PathBuf,

    /// Run every case this many times, reporting a success rate instead of a
    /// plain pass/fail — useful for non-deterministic models/`llm_judge`.
    #[arg(long, default_value_t = 1)]
    pub(crate) repeat: u32,

    /// Output format.
    #[arg(long, value_enum, default_value_t = EvalFormat::Text)]
    pub(crate) format: EvalFormat,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum EvalFormat {
    /// A per-case success-rate report with failing assertions detailed.
    #[default]
    Text,
    /// A structured JSON report (one entry per case, with every repeat's
    /// outcome).
    Json,
}

#[derive(Debug, Args)]
pub(crate) struct CacheCommand {
    #[command(subcommand)]
    pub(crate) action: CacheAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CacheAction {
    /// Delete every cached response under `.lait/cache/`.
    Clear,
}

#[derive(Debug, Args)]
pub(crate) struct WorkflowCommand {
    #[command(subcommand)]
    pub(crate) action: WorkflowAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum WorkflowAction {
    /// List every `workflows:` entry configured in lait.config.yml.
    List,
}

#[derive(Debug, Args)]
pub(crate) struct SkillCommand {
    #[command(subcommand)]
    pub(crate) action: SkillAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SkillAction {
    /// List every `skills:` entry configured in lait.config.yml.
    List,
}

#[derive(Debug, Args)]
pub(crate) struct RunsCommand {
    #[command(subcommand)]
    pub(crate) action: RunsAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RunsAction {
    /// List every checkpointed run under `.lait/runs/`.
    List,
    /// Print one checkpointed run's recorded state.
    Show(RunsIdArgs),
}

#[derive(Debug, Args)]
pub(crate) struct RunsIdArgs {
    /// The run id (as printed by `--checkpoint`/`lait runs list`).
    #[arg(value_name = "RUN_ID")]
    pub(crate) run_id: String,
}

#[derive(Debug, Args)]
pub(crate) struct GraphArgs {
    /// Path to the workflow YAML file (e.g. workflow.yml).
    #[arg(value_name = "FILE")]
    pub(crate) file: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value_t = GraphFormat::Mermaid)]
    pub(crate) format: GraphFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum GraphFormat {
    /// Mermaid `flowchart` syntax, pastable as-is into a GitHub README/PR.
    Mermaid,
    /// Graphviz DOT syntax (`dot -Tpng`/`dot -Tsvg`).
    Dot,
}

#[derive(Debug, Args)]
pub(crate) struct HistoryArgs {
    #[command(subcommand)]
    pub(crate) action: Option<HistoryAction>,

    /// Maximum number of entries to show when listing (most recent first).
    #[arg(long, short = 'l', default_value_t = 20)]
    pub(crate) limit: usize,
}

#[derive(Debug, Subcommand)]
pub(crate) enum HistoryAction {
    /// Print the full prompt/response of history entry N (`1` = most recent,
    /// matching the numbering `lait history`/`lait history search` show).
    Show(HistoryShowArgs),
    /// List every entry whose prompt or response contains QUERY
    /// (case-insensitive).
    Search(HistorySearchArgs),
}

#[derive(Debug, Args)]
pub(crate) struct HistoryShowArgs {
    #[arg(value_name = "N")]
    pub(crate) index: usize,
}

#[derive(Debug, Args)]
pub(crate) struct HistorySearchArgs {
    #[arg(value_name = "QUERY")]
    pub(crate) query: String,
}

#[derive(Debug, Args)]
pub(crate) struct PromptCommand {
    #[command(subcommand)]
    pub(crate) action: PromptAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum PromptAction {
    /// List every `prompts:` entry configured in lait.config.yml.
    List,
    /// Run a named prompt template from `prompts:` in lait.config.yml.
    Run(PromptRunArgs),
}

#[derive(Debug, Args)]
pub(crate) struct PromptRunArgs {
    /// Name of a `prompts:` entry in lait.config.yml.
    #[arg(value_name = "NAME")]
    pub(crate) name: String,

    /// The input passed to the prompt template (exposed as `{{ input }}`).
    /// May be omitted when input is piped via stdin.
    #[arg(value_name = "INPUT")]
    pub(crate) input: Option<String>,

    #[command(flatten)]
    pub(crate) var: VarArgs,

    #[command(flatten)]
    pub(crate) reporting: ReportingArgs,

    #[command(flatten)]
    pub(crate) output: OutputArgs,
}

/// `--var KEY=VALUE`, shared by `lait prompt run <NAME>`, `-p`/
/// `--prompt-name` on single-shot chat, and `lait run` — every entry point
/// that renders a `{{ vars.<key> }}` template placeholder.
#[derive(Debug, Clone, Args)]
pub(crate) struct VarArgs {
    /// Set a template variable: `--var KEY=VALUE`. Repeatable; a later
    /// `--var` for the same key wins. For a named prompt (`lait prompt run
    /// <NAME>` or `-p`/`--prompt-name`), overrides that prompt's `vars:`
    /// default. For `lait run`, VALUE is parsed as JSON when possible
    /// (`--var items='["a","b"]'`), otherwise used as a plain string;
    /// exposed to step templates as `{{ vars.KEY }}` and to jq filters as
    /// `$vars.KEY`.
    #[arg(long = "var", value_name = "KEY=VALUE")]
    pub(crate) var: Vec<String>,
}

/// `--show-usage`/`--no-history`, shared by every `run_*` entry point that
/// records a `lait history` entry and can print a token usage summary.
#[derive(Debug, Clone, Args)]
pub(crate) struct ReportingArgs {
    /// Print a token usage summary to stderr when the run finishes (for
    /// servers that report usage).
    #[arg(long)]
    pub(crate) show_usage: bool,

    /// Do not record this run in `lait history`.
    #[arg(long)]
    pub(crate) no_history: bool,
}

/// `--base-url`/`--api-key`, shared by every command that talks to an
/// OpenAI-compatible endpoint directly.
#[derive(Debug, Clone, Args)]
pub(crate) struct EndpointArgs {
    /// The OpenAI-compatible API base URL.
    #[arg(long, env = "OPENAI_BASE_URL")]
    pub(crate) base_url: Option<String>,

    /// The API key. LM Studio does not require one.
    #[arg(long, env = "OPENAI_API_KEY")]
    pub(crate) api_key: Option<String>,
}

/// `-o`/`--render`/`--json`, shared by every subcommand that produces a
/// single finished response body: single-shot chat, `lait run`, `lait agent
/// run`, and `lait prompt run <NAME>`. Previously only `ChatArgs` had these (see
/// the design plan's B-2) — `--session` is deliberately not part of this
/// bundle, since a workflow/agent run has no single conversation turn to
/// append a session entry for.
#[derive(Debug, Clone, Args)]
pub(crate) struct OutputArgs {
    /// Write the response body to PATH instead of stdout (`-o -` writes to
    /// stdout explicitly). With `--json`, the JSON object goes to the file.
    /// Without `--stream` (single-shot chat only), the file is only written
    /// after the request succeeds, so a failed run leaves no empty file
    /// behind.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,

    /// Print the response as JSON (`{"content", "reasoning", "usage"}`;
    /// `reasoning` is always `null` outside single-shot chat).
    #[arg(long)]
    pub(crate) json: bool,

    /// Render the response as Markdown for terminal display (headings,
    /// lists, emphasis, code blocks, tables, ...) instead of printing it as
    /// raw text. Falls back to raw text automatically when stdout isn't a
    /// terminal, or when combined with `--stream` (single-shot chat only).
    /// Ignored with `--json`. Falls back to `default.render` in
    /// lait.config.yml when this is unset.
    #[arg(long)]
    pub(crate) render: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SessionsCommand {
    #[command(subcommand)]
    pub(crate) action: SessionsAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionsAction {
    /// List every saved session and how many turns it holds.
    List,
    /// Print every turn recorded for a session.
    Show(SessionsNameArgs),
    /// Delete a saved session.
    Delete(SessionsNameArgs),
}

#[derive(Debug, Args)]
pub(crate) struct SessionsNameArgs {
    /// The session name (as passed to `--session`).
    #[arg(value_name = "NAME")]
    pub(crate) name: String,
}

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// What to generate; omit for lait.config.yml in the current directory.
    #[arg(value_enum, value_name = "KIND")]
    pub(crate) kind: Option<InitKind>,

    /// Where to write the generated file (defaults: workflow.yml /
    /// agent.md). Only valid with a KIND — the config file's name is fixed.
    #[arg(value_name = "PATH", requires = "kind")]
    pub(crate) path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum InitKind {
    /// A workflow YAML scaffold (`nodes:` + `steps:`).
    Workflow,
    /// An agent Markdown scaffold (frontmatter + system prompt).
    Agent,
}

#[derive(Debug, Args)]
pub(crate) struct ManArgs {
    /// The directory the man pages are written into (created if missing).
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub(crate) dir: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct CompletionsArgs {
    /// The shell to generate a completion script for.
    #[arg(value_enum, value_name = "SHELL")]
    pub(crate) shell: clap_complete::Shell,
}

#[derive(Debug, Args)]
pub(crate) struct ModelsArgs {
    /// Ask the configured (or `--base-url`) server for its available models
    /// (`GET /v1/models`) instead of listing configured aliases.
    #[arg(long)]
    pub(crate) remote: bool,

    /// Print machine-readable JSON instead of a table.
    #[arg(long)]
    pub(crate) json: bool,

    #[command(flatten)]
    pub(crate) endpoint: EndpointArgs,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Path to the workflow YAML file (e.g. workflow.yml).
    #[arg(value_name = "FILE")]
    pub(crate) file: PathBuf,

    /// The initial input passed to the first step's `{{ input }}` placeholder.
    /// May be omitted when input is piped via stdin (which is then used as the
    /// input; when both are given, the piped text is appended to PROMPT).
    #[arg(value_name = "PROMPT")]
    pub(crate) prompt: Option<String>,

    /// Show the resolved execution plan (step order, resolved model/
    /// base_url, effective retry/timeout, and when/switch/parallel/loop/
    /// for_each structure) without calling a model, spawning an MCP server,
    /// or running a command. PROMPT/--var are used to render each step's
    /// template as far as they can be (see docs/usage/ja/workflow.md).
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Save a checkpoint after every top-level step completes
    /// (`.lait/runs/<run-id>.json`), so a failed run can be continued with
    /// `--resume <RUN_ID>` instead of starting over. The run id is printed
    /// to stderr; see `lait runs list`/`lait runs show`. Implied by
    /// `--resume` itself.
    #[arg(long)]
    pub(crate) checkpoint: bool,

    /// Resume a previously checkpointed run from its last completed
    /// top-level step (see `--checkpoint`, `lait runs list`). FILE must be
    /// the same workflow the run was checkpointed against; PROMPT is not
    /// used (the checkpoint already recorded the original one).
    #[arg(long, value_name = "RUN_ID", conflicts_with = "dry_run")]
    pub(crate) resume: Option<String>,

    /// Record every LLM request/response this run makes into DIR as cassette
    /// files (see `crate::cassette`), keyed by request content (the same
    /// hash `--cache` uses). A later `--replay DIR` run against the same
    /// workflow/input/vars answers each request from these cassettes instead
    /// of the network — see `lait test`/docs/usage/ja/testing.md.
    #[arg(long, value_name = "DIR", conflicts_with = "replay")]
    pub(crate) record: Option<PathBuf>,

    /// Replay a previously `--record`ed run from DIR instead of calling any
    /// model: every request this run would send is matched against DIR's
    /// cassette files by content hash, and the recorded response is returned
    /// with no network access at all. A request that has no matching
    /// cassette is an error (see `crate::cassette::load`).
    #[arg(long, value_name = "DIR", conflicts_with = "record")]
    pub(crate) replay: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) var: VarArgs,

    #[command(flatten)]
    pub(crate) reporting: ReportingArgs,

    #[command(flatten)]
    pub(crate) output: OutputArgs,
}

#[derive(Debug, Args)]
pub(crate) struct AgentCommand {
    #[command(subcommand)]
    pub(crate) action: AgentAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AgentAction {
    /// Run an agent Markdown file with the given input.
    Run(AgentRunArgs),
    /// List every `agents:` entry configured in lait.config.yml.
    List,
}

#[derive(Debug, Args)]
pub(crate) struct AgentRunArgs {
    /// Path to the agent Markdown file (frontmatter + system prompt template).
    #[arg(value_name = "FILE")]
    pub(crate) file: PathBuf,

    /// The input passed to the agent. Parsed as JSON when possible, so the
    /// system prompt template can access `{{ input.field }}`; otherwise used
    /// as-is for a plain `{{ input }}`. May be omitted when input is piped
    /// via stdin (which is then used as the input; when both are given, the
    /// piped text is appended to INPUT).
    #[arg(value_name = "INPUT")]
    pub(crate) input: Option<String>,

    #[command(flatten)]
    pub(crate) reporting: ReportingArgs,

    #[command(flatten)]
    pub(crate) output: OutputArgs,
}

#[derive(Debug, Args)]
pub(crate) struct LintArgs {
    /// Paths to workflow YAML files (.yml/.yaml), agent Markdown files
    /// (.md), and/or directories to search recursively for such files.
    /// Every file is checked even if an earlier one has errors. Inside a
    /// directory, a '.md' file is only checked when it starts with a '---'
    /// frontmatter delimiter (other Markdown is skipped); dot-directories
    /// (e.g. '.git'), 'target/', and 'node_modules/' are never descended
    /// into.
    #[arg(value_name = "PATH", required = true)]
    pub(crate) files: Vec<PathBuf>,

    /// Output format: 'text' (default, human-readable, one report per
    /// file), 'json' (a structured file/line/severity/message record per
    /// finding, for editor/CI tooling), or 'github' (GitHub Actions
    /// '::error file=...,line=...::'/'::warning ...::' annotations, so a
    /// finding is shown directly on the PR diff).
    #[arg(long, value_enum, default_value_t = LintFormat::Text)]
    pub(crate) format: LintFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum LintFormat {
    /// Human-readable per-file report (the default).
    Text,
    /// A JSON array of `{file, line, severity, message}` records.
    Json,
    /// GitHub Actions `::error`/`::warning` annotation lines.
    Github,
}

/// The chat options shared by single-shot chat (`ChatArgs`, flattened at the
/// top level) and the interactive REPL (`ChatReplArgs`, under `lait chat`):
/// everything about which model/endpoint/system-prompt/session a turn uses,
/// as opposed to `ChatArgs`' own fields, which only make sense for a single
/// request-and-exit invocation (`--json`, `--stream`, `-o`, `--json-schema`,
/// `--file`/`--image`, `-p`/`--var`).
#[derive(Debug, Clone, Args)]
pub(crate) struct SharedChatArgs {
    /// A configured model name or model identifier accepted by the server.
    #[arg(long, env = "LLM_MODEL")]
    pub(crate) model: Option<String>,

    #[command(flatten)]
    pub(crate) endpoint: EndpointArgs,

    /// A system prompt to send ahead of the user prompt. Falls back to
    /// `default.system` in lait.config.yml when neither this nor
    /// `--system-file` is given.
    #[arg(long, value_name = "TEXT", conflicts_with = "system_file")]
    pub(crate) system: Option<String>,

    /// Read the system prompt from FILE instead of the command line.
    #[arg(long, value_name = "FILE")]
    pub(crate) system_file: Option<PathBuf>,

    /// Display the model's reasoning content when the server provides it.
    #[arg(long)]
    pub(crate) show_reasoning: bool,

    #[command(flatten)]
    pub(crate) reporting: ReportingArgs,

    /// The reasoning effort to request from the model.
    #[arg(long, env = "LLM_REASONING_EFFORT", value_enum)]
    pub(crate) reasoning_effort: Option<ReasoningEffort>,

    /// Sampling temperature (0.0-2.0). Lower is more deterministic, higher is
    /// more random. Omitted from the request when unset.
    #[arg(long, env = "LLM_TEMPERATURE")]
    pub(crate) temperature: Option<f64>,

    /// Nucleus sampling probability mass (0.0-1.0), an alternative to
    /// `--temperature`. Omitted from the request when unset.
    #[arg(long, env = "LLM_TOP_P")]
    pub(crate) top_p: Option<f64>,

    /// An upper bound on the number of tokens generated for the completion.
    /// Omitted from the request when unset.
    #[arg(long, env = "LLM_MAX_TOKENS")]
    pub(crate) max_tokens: Option<u32>,

    /// Name of an `mcp_servers:` entry (from lait.config.yml) whose tools
    /// this request may call. Repeatable. Falls back to `default.mcp` in
    /// lait.config.yml when unset. Freely combinable with `--stream` — a
    /// streamed `tool_calls` field's fragments are reassembled per round;
    /// see `RequestSettings::complete_stream`.
    #[arg(long = "mcp", value_name = "NAME")]
    pub(crate) mcp: Vec<String>,

    /// Name of an `agents:` entry (from lait.config.yml) made available as a
    /// callable subagent tool this request may call. Repeatable. Falls back
    /// to `default.subagents` in lait.config.yml when unset. Freely
    /// combinable with `--stream`, like `--mcp` above.
    #[arg(long = "subagent", value_name = "NAME")]
    pub(crate) subagent: Vec<String>,

    /// Name of a `tools:` entry (from lait.config.yml) — a local command
    /// exposed as a callable tool without an MCP server — this request may
    /// call. Repeatable. Falls back to `default.tools` in lait.config.yml
    /// when unset. Freely combinable with `--stream`, like `--mcp` above.
    #[arg(long = "tool", value_name = "NAME")]
    pub(crate) tool: Vec<String>,

    /// Resume (or start) a named conversation: this call's prompt and the
    /// model's reply are appended to `.lait/sessions/<NAME>.jsonl`, and every
    /// turn recorded there so far is sent ahead of this call's prompt.
    #[arg(long, value_name = "NAME")]
    pub(crate) session: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ChatArgs {
    #[command(flatten)]
    pub(crate) shared: SharedChatArgs,

    #[command(flatten)]
    pub(crate) output: OutputArgs,

    /// Print nothing but the response body: notes outside it (reasoning
    /// display, usage display) are suppressed, overriding
    /// `--show-reasoning`/`--show-usage`.
    #[arg(long)]
    pub(crate) quiet: bool,

    /// Stream the response to stdout as it is generated, instead of waiting
    /// for the full completion. Incompatible with `--json`, which needs the
    /// full response to build its JSON object.
    #[arg(long, conflicts_with = "json")]
    pub(crate) stream: bool,

    /// Request a structured JSON response using the schema in FILE.
    #[arg(long, value_name = "FILE")]
    pub(crate) json_schema: Option<PathBuf>,

    /// The name of the structured output schema.
    #[arg(long, default_value = "structured_output", requires = "json_schema")]
    pub(crate) schema_name: String,

    /// A single prompt to send as a user message. May be omitted when input
    /// is piped via stdin (which is then sent as the prompt; when both are
    /// given, the piped text is appended to PROMPT as context). `-` reads
    /// the prompt from stdin explicitly.
    #[arg(value_name = "PROMPT")]
    pub(crate) prompt: Option<String>,

    /// Attach a file's contents as context (as a named fenced code block
    /// appended after the prompt). Repeatable.
    #[arg(long = "file", value_name = "PATH")]
    pub(crate) files: Vec<PathBuf>,

    /// Attach an image for a vision-capable model: a local file path (sent as
    /// a base64 data URL) or an `http(s)://` URL (sent as-is). Repeatable.
    #[arg(long = "image", value_name = "PATH_OR_URL")]
    pub(crate) images: Vec<String>,

    /// Run a named prompt template (a `prompts.<NAME>` entry in
    /// lait.config.yml) instead of sending PROMPT/stdin directly as the
    /// request text: PROMPT/stdin becomes the template's `{{ input }}`. See
    /// `lait prompt run <NAME>`/`lait prompt list` for the equivalent subcommand.
    #[arg(short = 'p', long = "prompt-name", value_name = "NAME")]
    pub(crate) prompt_name: Option<String>,

    #[command(flatten)]
    pub(crate) var: VarArgs,
}

/// `lait chat`'s own arguments: just the options a REPL turn can use — see
/// `SharedChatArgs`. There is no `--stream` flag here because the REPL
/// streams every turn by default, including a turn that calls
/// `--mcp`/`--subagent` tools — see `repl::run_turn`.
#[derive(Debug, Args)]
pub(crate) struct ChatReplArgs {
    #[command(flatten)]
    pub(crate) shared: SharedChatArgs,
}

#[cfg(test)]
mod tests;
