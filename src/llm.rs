use anyhow::Result;
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, ReasoningEffort as OpenAiReasoningEffort, ResponseFormat,
    },
};

use crate::{cli::ReasoningEffort, response::ChatCompletionResponse};

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

pub(crate) struct CompletionRequest<'a> {
    pub(crate) base_url: &'a str,
    pub(crate) api_key: &'a str,
    pub(crate) model_id: &'a str,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) response_format: Option<ResponseFormat>,
    pub(crate) prompt: &'a str,
}

pub(crate) async fn complete(request: CompletionRequest<'_>) -> Result<ChatCompletionResponse> {
    let config = OpenAIConfig::new()
        .with_api_base(request.base_url)
        .with_api_key(request.api_key);
    let client = Client::with_config(config);

    let user_message = ChatCompletionRequestUserMessageArgs::default()
        .content(request.prompt)
        .build()?;
    let mut chat_request = CreateChatCompletionRequestArgs::default();
    chat_request.model(request.model_id);
    chat_request.messages(vec![ChatCompletionRequestMessage::from(user_message)]);
    chat_request.stream(false);
    if let Some(reasoning_effort) = request.reasoning_effort {
        chat_request.reasoning_effort(OpenAiReasoningEffort::from(reasoning_effort));
    }
    if let Some(response_format) = request.response_format {
        chat_request.response_format(response_format);
    }
    let chat_request = chat_request.build()?;

    let response: ChatCompletionResponse = client.chat().create_byot(chat_request).await?;
    Ok(response)
}
