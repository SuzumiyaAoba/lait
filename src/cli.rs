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
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a YAML-defined workflow (see workflow.yml).
    Run(RunArgs),
    /// Work with agent Markdown files (frontmatter + system prompt template).
    Agent(AgentCommand),
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Path to the workflow YAML file (e.g. workflow.yml).
    #[arg(value_name = "FILE")]
    pub(crate) file: PathBuf,

    /// The initial input passed to the first step's `{{ input }}` placeholder.
    #[arg(value_name = "PROMPT")]
    pub(crate) prompt: String,
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
    /// as-is for a plain `{{ input }}`.
    #[arg(value_name = "INPUT")]
    pub(crate) input: String,
}

#[derive(Debug, Args)]
pub(crate) struct ChatArgs {
    /// A configured model name or model identifier accepted by the server.
    #[arg(long, env = "LLM_MODEL")]
    pub(crate) model: Option<String>,

    /// The OpenAI-compatible API base URL.
    #[arg(long, env = "OPENAI_BASE_URL")]
    pub(crate) base_url: Option<String>,

    /// The API key. LM Studio does not require one.
    #[arg(long, env = "OPENAI_API_KEY")]
    pub(crate) api_key: Option<String>,

    /// Display the model's reasoning content when the server provides it.
    #[arg(long)]
    pub(crate) show_reasoning: bool,

    /// Print the response as JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Request a structured JSON response using the schema in FILE.
    #[arg(long, value_name = "FILE")]
    pub(crate) json_schema: Option<PathBuf>,

    /// The name of the structured output schema.
    #[arg(long, default_value = "structured_output", requires = "json_schema")]
    pub(crate) schema_name: String,

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

    /// A single prompt to send as a user message.
    #[arg(value_name = "PROMPT")]
    pub(crate) prompt: Option<String>,
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
        assert_eq!(cli.chat.model.as_deref(), Some("local-model"));
        assert_eq!(
            cli.chat.base_url.as_deref(),
            Some("http://localhost:1234/v1")
        );
        assert_eq!(cli.chat.api_key.as_deref(), Some("test-key"));
        assert!(cli.chat.show_reasoning);
        assert_eq!(cli.chat.reasoning_effort, Some(ReasoningEffort::High));
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

        assert!(!cli.chat.show_reasoning);
        assert_eq!(cli.chat.reasoning_effort, None);
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
                cli.chat.reasoning_effort,
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

        assert_eq!(cli.chat.temperature, Some(0.7));
        assert_eq!(cli.chat.top_p, Some(0.9));
        assert_eq!(cli.chat.max_tokens, Some(256));
    }

    #[test]
    fn leaves_temperature_top_p_and_max_tokens_unset_by_default() {
        let cli = Cli::try_parse_from(["lait", "--model", "local-model", "hello"])
            .expect("valid CLI arguments should parse");

        assert!(cli.chat.temperature.is_none());
        assert!(cli.chat.top_p.is_none());
        assert!(cli.chat.max_tokens.is_none());
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
                assert_eq!(run_args.prompt, "hello world");
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
                assert_eq!(run_args.input, "hello");
            }
            _ => panic!("expected the agent run subcommand to be selected"),
        }
    }

    #[test]
    fn agent_run_subcommand_requires_file_and_input() {
        assert!(Cli::try_parse_from(["lait", "agent", "run"]).is_err());
        assert!(Cli::try_parse_from(["lait", "agent", "run", "agent.md"]).is_err());
    }

    #[test]
    fn run_subcommand_accepts_global_no_config_after_its_args() {
        let cli = Cli::try_parse_from(["lait", "run", "workflow.yml", "hello", "--no-config"])
            .expect("global flags should be accepted after subcommand arguments");

        assert!(cli.no_config);
    }

    #[test]
    fn run_subcommand_requires_file_and_prompt() {
        assert!(Cli::try_parse_from(["lait", "run"]).is_err());
        assert!(Cli::try_parse_from(["lait", "run", "workflow.yml"]).is_err());
    }
}
