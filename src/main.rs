use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, CreateChatCompletionResponse,
    },
};
use clap::Parser;

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

    /// A single prompt to send as a user message.
    #[arg(value_name = "PROMPT")]
    prompt: String,
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
    let request = CreateChatCompletionRequestArgs::default()
        .model(cli.model)
        .messages(vec![ChatCompletionRequestMessage::from(user_message)])
        .stream(false)
        .build()?;

    let response = client.chat().create(request).await?;
    let content = response_content(&response)?;
    println!("{content}");
    Ok(())
}

fn response_content(response: &CreateChatCompletionResponse) -> Result<&str, &'static str> {
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

#[cfg(test)]
mod tests {
    use super::{Cli, response_content};
    use async_openai::types::chat::CreateChatCompletionResponse;
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
            "hello",
        ])
        .expect("valid CLI arguments should parse");

        assert_eq!(cli.model, "local-model");
        assert_eq!(cli.base_url, "http://localhost:1234/v1");
        assert_eq!(cli.api_key.as_deref(), Some("test-key"));
        assert_eq!(cli.prompt, "hello");
    }

    #[test]
    fn requires_model_and_prompt() {
        assert!(Cli::try_parse_from(["lait", "hello"]).is_err());
        assert!(Cli::try_parse_from(["lait", "--model", "local-model"]).is_err());
    }

    #[test]
    fn rejects_empty_choices_or_content() {
        let no_choices =
            serde_json::from_value::<CreateChatCompletionResponse>(serde_json::json!({
                "id": "completion",
                "object": "chat.completion",
                "choices": [],
                "created": 0,
                "model": "local-model"
            }))
            .expect("response fixture should deserialize");
        assert_eq!(
            response_content(&no_choices),
            Err("API response contained no choices")
        );

        let no_content =
            serde_json::from_value::<CreateChatCompletionResponse>(serde_json::json!({
                "id": "completion",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": null}
                }],
                "created": 0,
                "model": "local-model"
            }))
            .expect("response fixture should deserialize");
        assert_eq!(
            response_content(&no_content),
            Err("API response contained no content in its first choice")
        );
    }
}
