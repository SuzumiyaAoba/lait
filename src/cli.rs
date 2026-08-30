use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Deserialize;

/// Lightweight AI Tool command-line interface.
#[derive(Debug, Parser)]
#[command(name = "lait", version, about = "Lightweight AI Tool")]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    #[command(flatten)]
    pub(crate) chat: ChatArgs,

    /// Do not read lait.config.yml from the current directory.
    #[arg(long, global = true)]
    pub(crate) no_config: bool,

    /// Do not read a `.env` file from the current directory. (Acted on
    /// before argument parsing — see `main` — so this declaration only
    /// provides `--help` output and validation.)
    #[arg(long, global = true)]
    pub(crate) no_env: bool,
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

    /// The OpenAI-compatible API base URL to query with `--remote`.
    #[arg(long, env = "OPENAI_BASE_URL")]
    pub(crate) base_url: Option<String>,

    /// The API key sent with `--remote`. LM Studio does not require one.
    #[arg(long, env = "OPENAI_API_KEY")]
    pub(crate) api_key: Option<String>,
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

    /// Print a per-step and total token usage summary to stderr when the
    /// workflow finishes (for servers that report usage).
    #[arg(long)]
    pub(crate) show_usage: bool,
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

    /// Print a token usage summary to stderr when the agent finishes (for
    /// servers that report usage).
    #[arg(long)]
    pub(crate) show_usage: bool,
}

#[derive(Debug, Args)]
pub(crate) struct LintArgs {
    /// Paths to workflow YAML files (.yml/.yaml) and/or agent Markdown files
    /// (.md) to check. Every file is checked even if an earlier one has
    /// errors.
    #[arg(value_name = "FILE", required = true)]
    pub(crate) files: Vec<PathBuf>,
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

    /// The OpenAI-compatible API base URL.
    #[arg(long, env = "OPENAI_BASE_URL")]
    pub(crate) base_url: Option<String>,

    /// The API key. LM Studio does not require one.
    #[arg(long, env = "OPENAI_API_KEY")]
    pub(crate) api_key: Option<String>,

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

    /// Print the request's token usage to stderr after the response (so
    /// piping stdout stays clean). With `--stream`, asks the server for
    /// `stream_options: {"include_usage": true}`.
    #[arg(long)]
    pub(crate) show_usage: bool,

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
    /// lait.config.yml when unset. Single-shot chat rejects combining this
    /// with `--stream` at request-resolve time rather than at parse time (a
    /// streamed `tool_calls` field arrives as fragments lait does not yet
    /// reassemble; see `RequestSettings::complete_stream`) — unlike
    /// `ChatArgs`' own flags, this field is also reachable from `lait chat`,
    /// which has no single `--stream` flag to declare a clap-level conflict
    /// against (see `docs/usage/ja/chat.md`).
    #[arg(long = "mcp", value_name = "NAME")]
    pub(crate) mcp: Vec<String>,

    /// Name of an `agents:` entry (from lait.config.yml) made available as a
    /// callable subagent tool this request may call. Repeatable. Falls back
    /// to `default.subagents` in lait.config.yml when unset. Same
    /// `--stream` caveat as `--mcp` above.
    #[arg(long = "subagent", value_name = "NAME")]
    pub(crate) subagent: Vec<String>,

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

    /// Write the response body to PATH instead of stdout (`-o -` writes to
    /// stdout explicitly). With `--json`, the JSON object goes to the file;
    /// with `--show-reasoning`, reasoning goes to stderr so the file holds
    /// the body alone. Without `--stream`, the file is only written after
    /// the request succeeds, so a failed run leaves no empty file behind.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,

    /// Print nothing but the response body: notes outside it (reasoning
    /// display, usage display) are suppressed, overriding
    /// `--show-reasoning`/`--show-usage`.
    #[arg(long)]
    pub(crate) quiet: bool,

    /// Print the response as JSON.
    #[arg(long, conflicts_with = "stream")]
    pub(crate) json: bool,

    /// Stream the response to stdout as it is generated, instead of waiting
    /// for the full completion. Incompatible with `--json`, which needs the
    /// full response to build its JSON object.
    #[arg(long)]
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
}

/// `lait chat`'s own arguments: just the options a REPL turn can use — see
/// `SharedChatArgs`. There is no `--stream` flag here because the REPL
/// streams every turn by default (falling back to a non-streamed request
/// only when `--mcp`/`--subagent` is set — see `repl::run`).
#[derive(Debug, Args)]
pub(crate) struct ChatReplArgs {
    #[command(flatten)]
    pub(crate) shared: SharedChatArgs,
}

impl ReasoningEffort {
    /// The lowercase name used on the CLI and in YAML, for display (e.g.
    /// `lait models`' DEFAULTS column). Must match the `#[value(name)]`
    /// attributes below — pinned by `as_str_matches_the_clap_value_names`
    /// (`&'static str` is why this can't just call `to_possible_value`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReasoningEffort {
    #[value(name = "none")]
    #[serde(rename = "none")]
    None,
    #[value(name = "minimal")]
    #[serde(rename = "minimal")]
    Minimal,
    #[value(name = "low")]
    #[serde(rename = "low")]
    Low,
    #[value(name = "medium")]
    #[serde(rename = "medium")]
    Medium,
    #[value(name = "high")]
    #[serde(rename = "high")]
    High,
    #[value(name = "xhigh")]
    #[serde(rename = "xhigh")]
    Xhigh,
}

#[cfg(test)]
mod tests {
    use super::{AgentAction, AgentCommand, Cli, Command, ReasoningEffort};
    use clap::Parser;

    #[test]
    fn parses_prompt_and_options() {
        let cli = Cli::try_parse_from([
            "lait",
            "--model",
            "local-model",
            "--base-url",
            "http://localhost:1234/v1",
            "--api-key",
            "test-key",
            "--show-reasoning",
            "--reasoning-effort",
            "high",
            "hello",
        ])
        .expect("valid CLI arguments should parse");

        assert!(cli.command.is_none());
        assert_eq!(cli.chat.shared.model.as_deref(), Some("local-model"));
        assert_eq!(
            cli.chat.shared.base_url.as_deref(),
            Some("http://localhost:1234/v1")
        );
        assert_eq!(cli.chat.shared.api_key.as_deref(), Some("test-key"));
        assert!(cli.chat.shared.show_reasoning);
        assert_eq!(
            cli.chat.shared.reasoning_effort,
            Some(ReasoningEffort::High)
        );
        assert_eq!(cli.chat.prompt.as_deref(), Some("hello"));
        assert!(cli.chat.json_schema.is_none());
        assert_eq!(cli.chat.schema_name, "structured_output");
    }

    #[test]
    fn parses_json_schema_options_with_default_name() {
        let cli = Cli::try_parse_from([
            "lait",
            "--model",
            "local-model",
            "--json-schema",
            "schema.json",
            "hello",
        ])
        .expect("valid JSON schema arguments should parse");

        assert_eq!(
            cli.chat
                .json_schema
                .as_deref()
                .and_then(|path| path.to_str()),
            Some("schema.json")
        );
        assert_eq!(cli.chat.schema_name, "structured_output");
    }

    #[test]
    fn hides_reasoning_by_default() {
        let cli = Cli::try_parse_from(["lait", "--model", "local-model", "hello"])
            .expect("valid CLI arguments should parse");

        assert!(!cli.chat.shared.show_reasoning);
        assert_eq!(cli.chat.shared.reasoning_effort, None);
    }

    #[test]
    fn as_str_matches_the_clap_value_names() {
        use clap::ValueEnum;

        for variant in ReasoningEffort::value_variants() {
            assert_eq!(
                variant.as_str(),
                variant
                    .to_possible_value()
                    .expect("no reasoning effort variant is skipped")
                    .get_name(),
                "ReasoningEffort::as_str drifted from the #[value(name)] attribute"
            );
        }
    }

    #[test]
    fn accepts_all_reasoning_effort_values() {
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh"] {
            let cli = Cli::try_parse_from([
                "lait",
                "--model",
                "local-model",
                "--reasoning-effort",
                effort,
                "hello",
            ])
            .expect("reasoning effort should be accepted");

            assert_eq!(
                cli.chat.shared.reasoning_effort,
                Some(match effort {
                    "none" => ReasoningEffort::None,
                    "minimal" => ReasoningEffort::Minimal,
                    "low" => ReasoningEffort::Low,
                    "medium" => ReasoningEffort::Medium,
                    "high" => ReasoningEffort::High,
                    "xhigh" => ReasoningEffort::Xhigh,
                    _ => unreachable!(),
                })
            );
        }
    }

    #[test]
    fn parses_temperature_top_p_and_max_tokens() {
        let cli = Cli::try_parse_from([
            "lait",
            "--model",
            "local-model",
            "--temperature",
            "0.7",
            "--top-p",
            "0.9",
            "--max-tokens",
            "256",
            "hello",
        ])
        .expect("valid sampling options should parse");

        assert_eq!(cli.chat.shared.temperature, Some(0.7));
        assert_eq!(cli.chat.shared.top_p, Some(0.9));
        assert_eq!(cli.chat.shared.max_tokens, Some(256));
    }

    #[test]
    fn leaves_temperature_top_p_and_max_tokens_unset_by_default() {
        let cli = Cli::try_parse_from(["lait", "--model", "local-model", "hello"])
            .expect("valid CLI arguments should parse");

        assert!(cli.chat.shared.temperature.is_none());
        assert!(cli.chat.shared.top_p.is_none());
        assert!(cli.chat.shared.max_tokens.is_none());
    }

    #[test]
    fn parses_stream_flag() {
        let cli = Cli::try_parse_from(["lait", "--model", "local-model", "--stream", "hello"])
            .expect("valid CLI arguments should parse");

        assert!(cli.chat.stream);
    }

    #[test]
    fn leaves_stream_off_by_default() {
        let cli = Cli::try_parse_from(["lait", "--model", "local-model", "hello"])
            .expect("valid CLI arguments should parse");

        assert!(!cli.chat.stream);
    }

    #[test]
    fn rejects_stream_combined_with_json() {
        assert!(
            Cli::try_parse_from([
                "lait",
                "--model",
                "local-model",
                "--json",
                "--stream",
                "hello",
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_reasoning_effort_value() {
        assert!(
            Cli::try_parse_from([
                "lait",
                "--model",
                "local-model",
                "--reasoning-effort",
                "extreme",
                "hello",
            ])
            .is_err()
        );
    }

    #[test]
    fn allows_model_from_config_and_leaves_prompt_optional_for_app_level_validation() {
        // PROMPT is optional at the clap level so that a subcommand (e.g. `run`) can be
        // used instead; app-level code enforces that chat mode requires it.
        assert!(Cli::try_parse_from(["lait", "hello"]).is_ok());
        let cli = Cli::try_parse_from(["lait", "--model", "local-model"])
            .expect("prompt-less invocation should still parse");
        assert!(cli.chat.prompt.is_none());
    }

    #[test]
    fn parses_run_subcommand() {
        let cli = Cli::try_parse_from(["lait", "run", "workflow.yml", "hello world"])
            .expect("valid run subcommand arguments should parse");

        match cli.command {
            Some(Command::Run(run_args)) => {
                assert_eq!(run_args.file.to_str(), Some("workflow.yml"));
                assert_eq!(run_args.prompt.as_deref(), Some("hello world"));
            }
            _ => panic!("expected the run subcommand to be selected"),
        }
    }

    #[test]
    fn parses_agent_run_subcommand() {
        let cli = Cli::try_parse_from(["lait", "agent", "run", "agent.md", "hello"])
            .expect("valid agent run subcommand arguments should parse");

        match cli.command {
            Some(Command::Agent(AgentCommand {
                action: AgentAction::Run(run_args),
            })) => {
                assert_eq!(run_args.file.to_str(), Some("agent.md"));
                assert_eq!(run_args.input.as_deref(), Some("hello"));
            }
            _ => panic!("expected the agent run subcommand to be selected"),
        }
    }

    #[test]
    fn agent_run_subcommand_requires_a_file_but_not_an_input() {
        // INPUT is optional at the clap level so it can come from piped
        // stdin instead; app-level code enforces that one of the two exists.
        assert!(Cli::try_parse_from(["lait", "agent", "run"]).is_err());
        let cli = Cli::try_parse_from(["lait", "agent", "run", "agent.md"])
            .expect("input-less agent run should still parse");
        match cli.command {
            Some(Command::Agent(AgentCommand {
                action: AgentAction::Run(run_args),
            })) => assert!(run_args.input.is_none()),
            _ => panic!("expected the agent run subcommand to be selected"),
        }
    }

    #[test]
    fn run_subcommand_accepts_global_no_config_after_its_args() {
        let cli = Cli::try_parse_from(["lait", "run", "workflow.yml", "hello", "--no-config"])
            .expect("global flags should be accepted after subcommand arguments");

        assert!(cli.no_config);
    }

    #[test]
    fn run_subcommand_requires_a_file_but_not_a_prompt() {
        // PROMPT is optional at the clap level so it can come from piped
        // stdin instead; app-level code enforces that one of the two exists.
        assert!(Cli::try_parse_from(["lait", "run"]).is_err());
        let cli = Cli::try_parse_from(["lait", "run", "workflow.yml"])
            .expect("prompt-less run should still parse");
        match cli.command {
            Some(Command::Run(run_args)) => assert!(run_args.prompt.is_none()),
            _ => panic!("expected the run subcommand to be selected"),
        }
    }

    #[test]
    fn parses_lint_subcommand_with_a_single_file() {
        let cli = Cli::try_parse_from(["lait", "lint", "workflow.yml"])
            .expect("valid lint subcommand arguments should parse");

        match cli.command {
            Some(Command::Lint(lint_args)) => {
                assert_eq!(lint_args.files.len(), 1);
                assert_eq!(lint_args.files[0].to_str(), Some("workflow.yml"));
            }
            _ => panic!("expected the lint subcommand to be selected"),
        }
    }

    #[test]
    fn parses_lint_subcommand_with_multiple_files() {
        let cli = Cli::try_parse_from(["lait", "lint", "workflow.yml", "agent.md"])
            .expect("valid lint subcommand arguments should parse");

        match cli.command {
            Some(Command::Lint(lint_args)) => {
                assert_eq!(
                    lint_args
                        .files
                        .iter()
                        .filter_map(|path| path.to_str())
                        .collect::<Vec<_>>(),
                    vec!["workflow.yml", "agent.md"]
                );
            }
            _ => panic!("expected the lint subcommand to be selected"),
        }
    }

    #[test]
    fn lint_subcommand_requires_at_least_one_file() {
        assert!(Cli::try_parse_from(["lait", "lint"]).is_err());
    }

    #[test]
    fn lint_subcommand_accepts_global_no_config() {
        let cli = Cli::try_parse_from(["lait", "lint", "workflow.yml", "--no-config"])
            .expect("global flags should be accepted after subcommand arguments");

        assert!(cli.no_config);
    }
}
