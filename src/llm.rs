use anyhow::{Result, bail};
use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
        ChatCompletionRequestUserMessageArgs, ChatCompletionRequestUserMessageContentPart,
        ChatCompletionStreamOptions, ChatCompletionTools, CreateChatCompletionRequest,
        CreateChatCompletionRequestArgs, FunctionCall, ImageUrl,
        ReasoningEffort as OpenAiReasoningEffort, ResponseFormat,
    },
};
use futures_util::Stream;

use crate::{
    cli::ReasoningEffort,
    response::{ChatCompletionResponse, ChatCompletionStreamChunk, ToolCall},
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
    /// The full message history for this request: for a single-shot call
    /// this is just `initial_messages(system_prompt, prompt)`, but a tool
    /// loop (`app::RequestSettings::complete`) grows it across rounds with
    /// the assistant's `tool_calls` message and each tool's `tool`-role
    /// result. Owned (not built from `system_prompt`/`prompt` here) so the
    /// caller can reuse/extend the same history across rounds without lait
    /// re-deriving it each time.
    pub(crate) messages: Vec<ChatCompletionRequestMessage>,
    /// The MCP-derived tools available to the model this round. Empty means
    /// "don't send a `tools:` field at all", not "send an empty list" —
    /// some servers treat the two differently.
    pub(crate) tools: &'a [ChatCompletionTools],
    /// Ask a streamed response to append a final, choiceless chunk carrying
    /// the whole request's `usage` (`stream_options: {"include_usage":
    /// true}`). Only meaningful for [`complete_stream`]; ignored by
    /// [`complete`], whose response carries `usage` unconditionally. Off by
    /// default — only set when the caller will actually read the usage, so
    /// servers that don't know `stream_options` aren't sent one needlessly.
    pub(crate) stream_include_usage: bool,
}

/// Builds the initial message history shared by every completion request
/// (chat/agent/workflow-step): an optional system prompt, then `history`
/// (prior turns of a resumed `--session`, empty for everyone else), then the
/// new user turn built from `prompt`/`image_urls`. A tool loop starts from
/// this and appends to it round by round.
pub(crate) fn initial_messages(
    system_prompt: Option<&str>,
    history: &[ChatCompletionRequestMessage],
    prompt: &str,
    image_urls: &[String],
) -> Result<Vec<ChatCompletionRequestMessage>> {
    let mut messages = Vec::with_capacity(2 + history.len());
    if let Some(system_prompt) = system_prompt {
        let system_message = ChatCompletionRequestSystemMessageArgs::default()
            .content(system_prompt)
            .build()?;
        messages.push(ChatCompletionRequestMessage::from(system_message));
    }
    messages.extend(history.iter().cloned());
    messages.push(user_message(prompt, image_urls)?);
    Ok(messages)
}

/// Builds a single user-role message from `prompt`, attaching `image_urls`
/// (each already a `data:` URL or a plain `http(s)://` URL — see
/// `attachment::resolve_image_urls`) as `image_url` content parts alongside
/// the text when non-empty. Empty `image_urls` keeps the plain-text `content`
/// shape every request used before `--image` existed, so a server that only
/// understands a bare string content still works unchanged.
pub(crate) fn user_message(
    prompt: &str,
    image_urls: &[String],
) -> Result<ChatCompletionRequestMessage> {
    if image_urls.is_empty() {
        let user_message = ChatCompletionRequestUserMessageArgs::default()
            .content(prompt)
            .build()?;
        return Ok(ChatCompletionRequestMessage::from(user_message));
    }

    let mut parts = Vec::with_capacity(1 + image_urls.len());
    parts.push(ChatCompletionRequestUserMessageContentPart::Text(
        ChatCompletionRequestMessageContentPartText {
            text: prompt.to_owned(),
        },
    ));
    for url in image_urls {
        parts.push(ChatCompletionRequestUserMessageContentPart::ImageUrl(
            ChatCompletionRequestMessageContentPartImage {
                image_url: ImageUrl {
                    url: url.clone(),
                    detail: None,
                },
            },
        ));
    }
    let user_message = ChatCompletionRequestUserMessageArgs::default()
        .content(parts)
        .build()?;
    Ok(ChatCompletionRequestMessage::from(user_message))
}

/// Builds the assistant-role message recording a model turn's `tool_calls`
/// (and any `content` it produced alongside them), the shape a tool loop
/// (`app::RequestSettings::complete`) appends to its message history right
/// before running the calls themselves.
pub(crate) fn assistant_tool_call_message(
    tool_calls: &[ToolCall],
    content: Option<&str>,
) -> Result<ChatCompletionRequestMessage> {
    let tool_call_entries: Vec<ChatCompletionMessageToolCalls> = tool_calls
        .iter()
        .map(|tool_call| {
            ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                id: tool_call.id.clone(),
                function: FunctionCall {
                    name: tool_call.function.name.clone(),
                    arguments: tool_call.function.arguments.clone(),
                },
            })
        })
        .collect();
    let mut assistant_message = ChatCompletionRequestAssistantMessageArgs::default();
    assistant_message.tool_calls(tool_call_entries);
    if let Some(content) = content {
        assistant_message.content(content);
    }
    Ok(ChatCompletionRequestMessage::from(
        assistant_message.build()?,
    ))
}

/// Builds the `tool`-role message carrying one tool call's result back to the
/// model, the shape a tool loop appends to its message history for each call
/// a round makes.
pub(crate) fn tool_result_message(
    tool_call_id: &str,
    content: String,
) -> Result<ChatCompletionRequestMessage> {
    let tool_message = ChatCompletionRequestToolMessageArgs::default()
        .content(content)
        .tool_call_id(tool_call_id)
        .build()?;
    Ok(ChatCompletionRequestMessage::from(tool_message))
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

/// Checks a `max_tool_rounds` value (a CLI/agent-file/node/workflow-default
/// setting, or the value once every fallback layer has resolved it) is at
/// least 1, the same "validate eagerly everywhere, then again once resolved"
/// pattern as [`validate_sampling_params`]. Called from `agent::parse_agent`,
/// `workflow::validate::validate_node`/`validate_workflow_defaults`, and
/// `app::resolve_request_settings`.
pub(crate) fn validate_max_tool_rounds(
    max_tool_rounds: Option<usize>,
    description: &str,
) -> Result<()> {
    if max_tool_rounds == Some(0) {
        bail!("{description} has 'max_tool_rounds: 0'; it must be at least 1");
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
/// the caller passes the result to. Takes `request` by value (rather than
/// `&CompletionRequest`) so `messages`/`response_format` can be moved into
/// the builder instead of cloned — the caller never uses `request` again
/// after this call.
fn build_chat_request(
    request: CompletionRequest<'_>,
    stream: bool,
) -> Result<CreateChatCompletionRequest> {
    let mut chat_request = CreateChatCompletionRequestArgs::default();
    chat_request.model(request.model_id);
    chat_request.messages(request.messages);
    chat_request.stream(stream);
    if stream && request.stream_include_usage {
        chat_request.stream_options(ChatCompletionStreamOptions {
            include_usage: Some(true),
            include_obfuscation: None,
        });
    }
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
    if let Some(response_format) = request.response_format {
        chat_request.response_format(response_format);
    }
    if !request.tools.is_empty() {
        chat_request.tools(request.tools.to_vec());
    }
    Ok(chat_request.build()?)
}

/// The one HTTP client every completion request goes through. reqwest pools
/// connections per (scheme, host) inside a client, so sharing one across a
/// tool loop's rounds, a workflow's steps, and retries reuses live
/// connections instead of paying a fresh TCP (and, for HTTPS, TLS) handshake
/// per request — `Client::with_config` alone would build a new pool each
/// call.
static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

/// The shared HTTP client, for API calls outside the chat completions path
/// (`lait models --remote`'s `GET /v1/models`), so they reuse the same
/// connection pool completions do.
pub(crate) fn http_client() -> reqwest::Client {
    HTTP_CLIENT.clone()
}

/// Builds the API client [`complete`] and [`complete_stream`] share, wiring
/// the request's base URL/API key to the shared `HTTP_CLIENT`.
fn client(base_url: &str, api_key: &str) -> Client<OpenAIConfig> {
    let config = OpenAIConfig::new()
        .with_api_base(base_url)
        .with_api_key(api_key);
    Client::with_config(config).with_http_client(HTTP_CLIENT.clone())
}

pub(crate) async fn complete(request: CompletionRequest<'_>) -> Result<ChatCompletionResponse> {
    let client = client(request.base_url, request.api_key);
    let chat_request = build_chat_request(request, false)?;
    let response: ChatCompletionResponse = client.chat().create_byot(chat_request).await?;
    Ok(response)
}

/// Like [`complete`], but requests `stream: true` and returns the parsed SSE
/// stream of response chunks instead of waiting for the full response.
pub(crate) async fn complete_stream(request: CompletionRequest<'_>) -> Result<CompletionStream> {
    let client = client(request.base_url, request.api_key);
    let chat_request = build_chat_request(request, true)?;
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
