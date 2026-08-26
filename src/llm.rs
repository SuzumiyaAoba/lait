use anyhow::{Result, bail};
use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequest,
        CreateChatCompletionRequestArgs, ReasoningEffort as OpenAiReasoningEffort, ResponseFormat,
    },
};
use futures_util::Stream;

use crate::{
    cli::ReasoningEffort,
    response::{ChatCompletionResponse, ChatCompletionStreamChunk},
};

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
    /// Sampling temperature (0.0-2.0), forwarded as-is; see
    /// `validate_sampling_params` for the range check applied before a
    /// request is ever built.
    pub(crate) temperature: Option<f64>,
    /// Nucleus sampling probability mass (0.0-1.0).
    pub(crate) top_p: Option<f64>,
    /// An upper bound on the number of tokens generated for the completion,
    /// sent as the (non-deprecated) `max_completion_tokens` field.
    pub(crate) max_tokens: Option<u32>,
    pub(crate) response_format: Option<ResponseFormat>,
    /// An optional system-role message sent before the user prompt, e.g. an
    /// agent file's rendered system prompt template.
    pub(crate) system_prompt: Option<&'a str>,
    pub(crate) prompt: &'a str,
}

/// Checks `temperature`/`top_p`/`max_tokens` are within the bounds the OpenAI
/// chat completions API documents (`temperature`: 0.0-2.0, `top_p`: 0.0-1.0,
/// `max_tokens`: at least 1), regardless of which layer they were resolved
/// from (CLI/env, a `models:` alias, `default:`, or a step/agent override).
/// Called both eagerly at workflow parse time (`workflow::validate`, so a bad
/// value fails before any step runs) and again once every fallback layer has
/// been resolved (`app::resolve_request_settings`, which also covers values
/// that only workflow parsing can't see, like a config file's `models:`).
pub(crate) fn validate_sampling_params(
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<u32>,
    description: &str,
) -> Result<()> {
    if let Some(temperature) = temperature
        && !(0.0..=2.0).contains(&temperature)
    {
        bail!("{description} has 'temperature: {temperature}'; it must be between 0.0 and 2.0");
    }
    if let Some(top_p) = top_p
        && !(0.0..=1.0).contains(&top_p)
    {
        bail!("{description} has 'top_p: {top_p}'; it must be between 0.0 and 1.0");
    }
    if max_tokens == Some(0) {
        bail!("{description} has 'max_tokens: 0'; it must be at least 1");
    }
    Ok(())
}

/// A parsed SSE stream of `chat.completion.chunk` events, as returned by
/// [`complete_stream`]. Each item is `Err` when a chunk fails to parse or the
/// connection drops mid-stream.
pub(crate) type CompletionStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<ChatCompletionStreamChunk, OpenAIError>> + Send>>;

/// Builds the request body shared by [`complete`] and [`complete_stream`];
/// the two differ only in `stream` and in which `Chat::create*_byot` method
/// the caller passes the result to.
fn build_chat_request(
    request: &CompletionRequest<'_>,
    stream: bool,
) -> Result<CreateChatCompletionRequest> {
    let mut messages = Vec::with_capacity(2);
    if let Some(system_prompt) = request.system_prompt {
        let system_message = ChatCompletionRequestSystemMessageArgs::default()
            .content(system_prompt)
            .build()?;
        messages.push(ChatCompletionRequestMessage::from(system_message));
    }
    let user_message = ChatCompletionRequestUserMessageArgs::default()
        .content(request.prompt)
        .build()?;
    messages.push(ChatCompletionRequestMessage::from(user_message));

    let mut chat_request = CreateChatCompletionRequestArgs::default();
    chat_request.model(request.model_id);
    chat_request.messages(messages);
    chat_request.stream(stream);
    if let Some(reasoning_effort) = request.reasoning_effort {
        chat_request.reasoning_effort(OpenAiReasoningEffort::from(reasoning_effort));
    }
    if let Some(temperature) = request.temperature {
        chat_request.temperature(temperature as f32);
    }
    if let Some(top_p) = request.top_p {
        chat_request.top_p(top_p as f32);
    }
    if let Some(max_tokens) = request.max_tokens {
        chat_request.max_completion_tokens(max_tokens);
    }
    if let Some(response_format) = request.response_format.clone() {
        chat_request.response_format(response_format);
    }
    Ok(chat_request.build()?)
}

pub(crate) async fn complete(request: CompletionRequest<'_>) -> Result<ChatCompletionResponse> {
    let config = OpenAIConfig::new()
        .with_api_base(request.base_url)
        .with_api_key(request.api_key);
    let client = Client::with_config(config);

    let chat_request = build_chat_request(&request, false)?;
    let response: ChatCompletionResponse = client.chat().create_byot(chat_request).await?;
    Ok(response)
}

/// Like [`complete`], but requests `stream: true` and returns the parsed SSE
/// stream of response chunks instead of waiting for the full response.
pub(crate) async fn complete_stream(request: CompletionRequest<'_>) -> Result<CompletionStream> {
    let config = OpenAIConfig::new()
        .with_api_base(request.base_url)
        .with_api_key(request.api_key);
    let client = Client::with_config(config);

    let chat_request = build_chat_request(&request, true)?;
    let stream = client.chat().create_stream_byot(chat_request).await?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::validate_sampling_params;

    #[test]
    fn accepts_unset_or_in_range_values() {
        assert!(validate_sampling_params(None, None, None, "x").is_ok());
        assert!(validate_sampling_params(Some(0.0), Some(0.0), Some(1), "x").is_ok());
        assert!(validate_sampling_params(Some(2.0), Some(1.0), Some(u32::MAX), "x").is_ok());
        assert!(validate_sampling_params(Some(0.7), Some(0.9), Some(256), "x").is_ok());
    }

    #[test]
    fn rejects_an_out_of_range_temperature() {
        let error = validate_sampling_params(Some(2.1), None, None, "step 'x'").unwrap_err();
        assert!(error.to_string().contains("temperature"));

        assert!(validate_sampling_params(Some(-0.1), None, None, "x").is_err());
    }

    #[test]
    fn rejects_an_out_of_range_top_p() {
        let error = validate_sampling_params(None, Some(1.1), None, "step 'x'").unwrap_err();
        assert!(error.to_string().contains("top_p"));

        assert!(validate_sampling_params(None, Some(-0.1), None, "x").is_err());
    }

    #[test]
    fn rejects_a_zero_max_tokens() {
        let error = validate_sampling_params(None, None, Some(0), "step 'x'").unwrap_err();
        assert!(error.to_string().contains("max_tokens"));
    }
}
