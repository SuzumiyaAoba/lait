use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::Deserialize;

/// Lightweight AI Tool command-line interface.
#[derive(Debug, Parser)]
#[command(name = "lait", version, about = "Lightweight AI Tool")]
pub(crate) struct Cli {
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

    /// Do not read lait.config.yml from the current directory.
    #[arg(long)]
    pub(crate) no_config: bool,

    /// A single prompt to send as a user message.
    #[arg(value_name = "PROMPT")]
    pub(crate) prompt: String,
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
    use super::{Cli, ReasoningEffort};
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

        assert_eq!(cli.model.as_deref(), Some("local-model"));
        assert_eq!(cli.base_url.as_deref(), Some("http://localhost:1234/v1"));
        assert_eq!(cli.api_key.as_deref(), Some("test-key"));
        assert!(cli.show_reasoning);
        assert_eq!(cli.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(cli.prompt, "hello");
        assert!(cli.json_schema.is_none());
        assert_eq!(cli.schema_name, "structured_output");
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
            cli.json_schema.as_deref().and_then(|path| path.to_str()),
            Some("schema.json")
        );
        assert_eq!(cli.schema_name, "structured_output");
    }

    #[test]
    fn hides_reasoning_by_default() {
        let cli = Cli::try_parse_from(["lait", "--model", "local-model", "hello"])
            .expect("valid CLI arguments should parse");

        assert!(!cli.show_reasoning);
        assert_eq!(cli.reasoning_effort, None);
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
                cli.reasoning_effort,
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
    fn requires_prompt_but_allows_model_from_config() {
        assert!(Cli::try_parse_from(["lait", "hello"]).is_ok());
        assert!(Cli::try_parse_from(["lait", "--model", "local-model"]).is_err());
    }
}
