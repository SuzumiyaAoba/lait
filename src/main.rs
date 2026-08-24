use anyhow::{Context, Result, anyhow, bail};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, ReasoningEffort as OpenAiReasoningEffort, ResponseFormat,
        ResponseFormatJsonSchema,
    },
};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

const DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";
const CONFIG_FILE_NAME: &str = "lait.config.yml";

/// Lightweight AI Tool command-line interface.
#[derive(Debug, Parser)]
#[command(name = "lait", version, about = "Lightweight AI Tool")]
struct Cli {
    /// A configured model name or model identifier accepted by the server.
    #[arg(long, env = "LLM_MODEL")]
    model: Option<String>,

    /// The OpenAI-compatible API base URL.
    #[arg(long, env = "OPENAI_BASE_URL")]
    base_url: Option<String>,

    /// The API key. LM Studio does not require one.
    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// Display the model's reasoning content when the server provides it.
    #[arg(long)]
    show_reasoning: bool,

    /// Print the response as JSON.
    #[arg(long)]
    json: bool,

    /// Request a structured JSON response using the schema in FILE.
    #[arg(long, value_name = "FILE")]
    json_schema: Option<PathBuf>,

    /// The name of the structured output schema.
    #[arg(long, default_value = "structured_output", requires = "json_schema")]
    schema_name: String,

    /// The reasoning effort to request from the model.
    #[arg(long, env = "LLM_REASONING_EFFORT", value_enum)]
    reasoning_effort: Option<ReasoningEffort>,

    /// Do not read lait.config.yml from the current directory.
    #[arg(long)]
    no_config: bool,

    /// A single prompt to send as a user message.
    #[arg(value_name = "PROMPT")]
    prompt: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, ValueEnum)]
enum ReasoningEffort {
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    models: HashMap<String, Vec<ModelDefinition>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelDefinition {
    provider: ProviderConfig,
    model_id: String,
    default_reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfig {
    base_url: String,
    api_key: Option<String>,
}

#[derive(Debug)]
struct ResolvedModel {
    model_id: String,
    base_url: Option<String>,
    api_key: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
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
    reasoning: Option<String>,
    reasoning_content: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    content: &'a str,
    reasoning: Option<&'a str>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("lait: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let file_config = load_config(cli.no_config)?;
    let model_name = cli
        .model
        .or_else(|| file_config.model.clone())
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "model is required; provide --model, set LLM_MODEL, or specify model in {CONFIG_FILE_NAME}"
            )
        })?;
    let resolved_model = resolve_model(model_name, &file_config)?;
    let base_url = cli
        .base_url
        .or(resolved_model.base_url)
        .or(file_config.base_url)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let base_url = base_url.trim_end_matches('/');
    if base_url.is_empty() {
        return Err(anyhow!("base URL must not be empty"));
    }

    let api_key = cli
        .api_key
        .or(resolved_model.api_key)
        .or(file_config.api_key)
        .unwrap_or_else(|| {
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

    let response_format = cli
        .json_schema
        .as_deref()
        .map(|path| load_json_schema(path, &cli.schema_name))
        .transpose()?;

    let user_message = ChatCompletionRequestUserMessageArgs::default()
        .content(cli.prompt)
        .build()?;
    let mut request = CreateChatCompletionRequestArgs::default();
    request.model(resolved_model.model_id);
    request.messages(vec![ChatCompletionRequestMessage::from(user_message)]);
    request.stream(false);
    if let Some(reasoning_effort) = cli
        .reasoning_effort
        .or(resolved_model.reasoning_effort)
        .or(file_config.reasoning_effort)
    {
        request.reasoning_effort(OpenAiReasoningEffort::from(reasoning_effort));
    }
    if let Some(response_format) = response_format {
        request.response_format(response_format);
    }
    let request = request.build()?;

    let response: ChatCompletionResponse = client.chat().create_byot(request).await?;
    let content = response_content(&response).map_err(anyhow::Error::msg)?;
    let reasoning = response_reasoning(&response);
    let output = if cli.json {
        serde_json::to_string(&JsonOutput { content, reasoning })?
    } else {
        format_response(content, reasoning, cli.show_reasoning)
    };
    println!("{output}");
    Ok(())
}

fn resolve_model(model_name: String, config: &ConfigFile) -> Result<ResolvedModel> {
    let Some(definitions) = config.models.get(&model_name) else {
        return Ok(ResolvedModel {
            model_id: model_name,
            base_url: None,
            api_key: None,
            reasoning_effort: None,
        });
    };
    let definition = definitions.first().ok_or_else(|| {
        anyhow!("model definition {model_name:?} must contain at least one entry")
    })?;
    if definition.model_id.trim().is_empty() {
        bail!("model_id in model definition {model_name:?} must not be empty");
    }

    Ok(ResolvedModel {
        model_id: definition.model_id.clone(),
        base_url: Some(definition.provider.base_url.clone()),
        api_key: definition.provider.api_key.clone(),
        reasoning_effort: definition.default_reasoning_effort,
    })
}

fn load_config(no_config: bool) -> Result<ConfigFile> {
    if no_config {
        return Ok(ConfigFile::default());
    }

    let path = std::env::current_dir()
        .context("failed to determine the current directory for configuration")?
        .join(CONFIG_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigFile::default());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read YAML configuration file '{}'",
                    path.display()
                )
            });
        }
    };

    serde_yaml::from_str(&contents).with_context(|| {
        format!(
            "failed to parse YAML configuration file '{}'",
            path.display()
        )
    })
}

fn load_json_schema(path: &Path, name: &str) -> Result<ResponseFormat> {
    if name.is_empty() || name.len() > 64 {
        bail!("JSON schema name must be between 1 and 64 characters: {name:?}");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!(
            "JSON schema name must contain only ASCII letters, digits, underscores, or hyphens: {name:?}"
        );
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read JSON schema file '{}'", path.display()))?;
    let schema = serde_json::from_str::<serde_json::Value>(&contents)
        .with_context(|| format!("failed to parse JSON schema file '{}'", path.display()))?;

    Ok(ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: None,
            name: name.to_owned(),
            schema,
            strict: Some(true),
        },
    })
}

fn response_content(response: &ChatCompletionResponse) -> std::result::Result<&str, &'static str> {
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
    response.choices.first().and_then(|choice| {
        choice
            .message
            .reasoning
            .as_deref()
            .filter(|reasoning| !reasoning.trim().is_empty())
            .or_else(|| {
                choice
                    .message
                    .reasoning_content
                    .as_deref()
                    .filter(|reasoning| !reasoning.trim().is_empty())
            })
    })
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
    use super::{
        ChatCompletionResponse, Cli, ReasoningEffort, format_response, response_content,
        response_reasoning,
    };
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

    #[test]
    fn reads_current_and_legacy_reasoning_fields() {
        let current = serde_json::from_value::<ChatCompletionResponse>(serde_json::json!({
            "choices": [{
                "message": {
                    "content": "answer",
                    "reasoning": "current reasoning",
                    "reasoning_content": "legacy reasoning"
                }
            }]
        }))
        .expect("response fixture should deserialize");
        assert_eq!(response_reasoning(&current), Some("current reasoning"));

        let legacy_fallback = serde_json::from_value::<ChatCompletionResponse>(serde_json::json!({
            "choices": [{
                "message": {
                    "content": "answer",
                    "reasoning": "  ",
                    "reasoning_content": "legacy reasoning"
                }
            }]
        }))
        .expect("response fixture should deserialize");
        assert_eq!(
            response_reasoning(&legacy_fallback),
            Some("legacy reasoning")
        );
    }
}
