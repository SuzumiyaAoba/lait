use anyhow::{Result, anyhow};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, ReasoningEffort as OpenAiReasoningEffort,
    },
};

use crate::{
    cli::{Cli, ReasoningEffort},
    config,
    response::ChatCompletionResponse,
    schema,
};

const DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";

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

pub(crate) async fn run(cli: Cli) -> Result<()> {
    let file_config = config::load_config(cli.no_config)?;
    let model_name = cli
        .model
        .or_else(|| file_config.model.clone())
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "model is required; provide --model, set LLM_MODEL, or specify model in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
    let resolved_model = config::resolve_model(model_name, &file_config)?;
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
        .map(|path| schema::load_json_schema(path, &cli.schema_name))
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
    let output = crate::response::render_response(&response, cli.json, cli.show_reasoning)?;
    println!("{output}");
    Ok(())
}
