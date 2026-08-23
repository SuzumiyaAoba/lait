use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, ReasoningEffort as OpenAiReasoningEffort,
    },
};
use clap::{Parser, ValueEnum};
use serde::Deserialize;

/// Lightweight AI Tool command-line interface.
#[derive(Debug, Parser)]
#[command(name = "lait", version, about = "Lightweight AI Tool")]
struct Cli {
    /// The model identifier accepted by the OpenAI-compatible server.
    #[arg(long, env = "LLM_MODEL")]
    model: String,

    /// The OpenAI-compatible API base URL.
    #[arg(
        long,
        env = "OPENAI_BASE_URL",
        default_value = "http://localhost:1234/v1"
    )]
    base_url: String,

    /// The API key. LM Studio does not require one.
    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// Display the model's reasoning content when the server provides it.
    #[arg(long)]
    show_reasoning: bool,

    /// The reasoning effort to request from the model.
    #[arg(long, env = "LLM_REASONING_EFFORT", value_enum)]
    reasoning_effort: Option<ReasoningEffort>,

    /// A single prompt to send as a user message.
    #[arg(value_name = "PROMPT")]
    prompt: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ReasoningEffort {
    #[value(name = "none")]
    None,
    #[value(name = "minimal")]
    Minimal,
    #[value(name = "low")]
    Low,
    #[value(name = "medium")]
    Medium,
    #[value(name = "high")]
    High,
    #[value(name = "xhigh")]
    Xhigh,
}

impl From<ReasoningEffort> for OpenAiReasoningEffort {
    fn from(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::None => Self::None,
            ReasoningEffort::Minimal => Self::Minimal,
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::Xhigh => Self::Xhigh,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    message: ChatCompletionResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponseMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("lait: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base_url = cli.base_url.trim_end_matches('/');
    if base_url.is_empty() {
        return Err("base URL must not be empty".into());
    }

    let api_key = cli.api_key.unwrap_or_else(|| {
        // async-openai always builds an Authorization header from its config.
        // LM Studio ignores the value, so use a non-empty dummy key when no
        // key was supplied instead of making local requests fail on an empty
        // header value.
        "lm-studio".to_owned()
    });
    let config = OpenAIConfig::new()
        .with_api_base(base_url)
        .with_api_key(api_key);
    let client = Client::with_config(config);

    let user_message = ChatCompletionRequestUserMessageArgs::default()
        .content(cli.prompt)
        .build()?;
    let mut request = CreateChatCompletionRequestArgs::default();
    request.model(cli.model);
    request.messages(vec![ChatCompletionRequestMessage::from(user_message)]);
    request.stream(false);
    if let Some(reasoning_effort) = cli.reasoning_effort {
        request.reasoning_effort(OpenAiReasoningEffort::from(reasoning_effort));
    }
    let request = request.build()?;

    let response: ChatCompletionResponse = client.chat().create_byot(request).await?;
    let content = response_content(&response)?;
    let output = format_response(content, response_reasoning(&response), cli.show_reasoning);
    println!("{output}");
    Ok(())
}

fn response_content(response: &ChatCompletionResponse) -> Result<&str, &'static str> {
    let choice = response
        .choices
        .first()
        .ok_or("API response contained no choices")?;
    choice
        .message
        .content
        .as_deref()
        .filter(|content| !content.is_empty())
        .ok_or("API response contained no content in its first choice")
}

fn response_reasoning(response: &ChatCompletionResponse) -> Option<&str> {
    response
        .choices
        .first()
        .and_then(|choice| choice.message.reasoning_content.as_deref())
}

fn format_response(content: &str, reasoning: Option<&str>, show_reasoning: bool) -> String {
    match (show_reasoning, reasoning) {
        (true, Some(reasoning)) if !reasoning.trim().is_empty() => {
            format!("Reasoning:\n{reasoning}\n\n{content}")
        }
        _ => content.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatCompletionResponse, Cli, ReasoningEffort, format_response, response_content};
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

        assert_eq!(cli.model, "local-model");
        assert_eq!(cli.base_url, "http://localhost:1234/v1");
        assert_eq!(cli.api_key.as_deref(), Some("test-key"));
        assert!(cli.show_reasoning);
        assert_eq!(cli.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(cli.prompt, "hello");
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

            assert_eq!(cli.reasoning_effort, Some(match effort {
                "none" => ReasoningEffort::None,
                "minimal" => ReasoningEffort::Minimal,
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                "xhigh" => ReasoningEffort::Xhigh,
                _ => unreachable!(),
            }));
        }
    }

    #[test]
    fn rejects_unknown_reasoning_effort_value() {
        assert!(Cli::try_parse_from([
            "lait",
            "--model",
            "local-model",
            "--reasoning-effort",
            "extreme",
            "hello",
        ])
        .is_err());
    }

    #[test]
    fn requires_model_and_prompt() {
        assert!(Cli::try_parse_from(["lait", "hello"]).is_err());
        assert!(Cli::try_parse_from(["lait", "--model", "local-model"]).is_err());
    }

    #[test]
    fn rejects_empty_choices_or_content() {
        let no_choices = serde_json::from_value::<ChatCompletionResponse>(serde_json::json!({
            "choices": []
        }))
        .expect("response fixture should deserialize");
        assert_eq!(
            response_content(&no_choices),
            Err("API response contained no choices")
        );

        let no_content = serde_json::from_value::<ChatCompletionResponse>(serde_json::json!({
            "choices": [{
                "message": {"content": null}
            }]
        }))
        .expect("response fixture should deserialize");
        assert_eq!(
            response_content(&no_content),
            Err("API response contained no content in its first choice")
        );
    }

    #[test]
    fn formats_reasoning_when_requested() {
        assert_eq!(
            format_response("answer", Some("step one"), true),
            "Reasoning:\nstep one\n\nanswer"
        );
        assert_eq!(format_response("answer", Some("  \n"), true), "answer");
        assert_eq!(format_response("answer", Some("step one"), false), "answer");
        assert_eq!(format_response("answer", None, true), "answer");
    }
}
