//! The Responses API (`POST /responses`) wire format — used instead of
//! `super::complete`'s Chat Completions format only for a model definition
//! whose `api:` is `responses` (see `config::ApiKind`). Reuses the exact
//! same [`super::CompletionRequest`] every Chat Completions call already
//! builds (`engine::transport` doesn't know or care which wire format
//! answers a request) and translates it in both directions: `messages`
//! (async-openai's own `ChatCompletionRequestMessage` enum) become the
//! Responses API's `instructions`/`input`, and a parsed reply is translated
//! back into lait's own [`response::ChatCompletionResponse`] — the same
//! type Chat Completions produces — so every downstream consumer (usage
//! tracking, `--trace-file`, rendering, `lait history`, the response disk
//! cache, `--record`/`--replay`) stays unaware which endpoint actually
//! answered.
//!
//! # Tool calling, images, and streaming
//!
//! The Chat Completions shapes lait's tool loop already produces are
//! translated one-to-one: `tools` (`{"type": "function", "function":
//! {...}}`) become the Responses API's flat `{"type": "function", "name",
//! ...}` definitions, an assistant message's `tool_calls` become one
//! `function_call` input item per call (after the message's own text, if
//! any), and each `tool`-role message becomes a `function_call_output` item
//! keyed by the same `call_id`. In the other direction, a reply's
//! `function_call` output items become `tool_calls` on the translated
//! message, so `engine::tool_loop` never learns which endpoint answered.
//! `--image` attachments (`image_url` content parts) become `input_image`
//! parts next to an `input_text` part, and a PDF `--file` (a `file` content
//! part) an `input_file` part. [`complete_stream`] translates the
//! Responses API's own typed SSE events (`response.output_text.delta`,
//! `response.function_call_arguments.delta`, ...) into the same
//! [`response::ChatCompletionStreamChunk`]s a Chat Completions stream
//! yields; see [`translate_stream_event`].
//!
//! # What this does not do
//!
//! `store` is always sent as `false` (no server-side retention) and
//! `previous_response_id`/`conversation` are never used — every round
//! resends the full message history, exactly like Chat Completions,
//! specifically so the disk cache/`--record`/`--replay` keep working
//! unmodified (see `cache::key`'s own doc comment, which this module does
//! not change). Reasoning output items are therefore not carried across a
//! tool loop's rounds either: lait's message history is the Chat
//! Completions shape, which has no place to keep them.

use anyhow::{Context, Result, anyhow, bail};
use async_openai::error::{OpenAIError, StreamError};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::response;

use super::{CompletionRequest, CompletionStream};

pub(crate) async fn complete(
    request: CompletionRequest<'_>,
) -> Result<response::ChatCompletionResponse> {
    let client = super::client(request.base_url, request.api_key);
    let cancellation = request.cancellation.clone();
    let body = build_request(&request, false)?;
    trace_request(&body);
    let raw: ResponsesApiResponse =
        super::await_cancellation(client.responses().create_byot(body), cancellation).await?;
    tracing::trace!(response = ?raw, "received responses API response");
    to_chat_completion_response(raw)
}

/// Like [`complete`], but sends `stream: true` and translates the
/// Responses API's typed SSE events into the Chat Completions chunk shape
/// `engine::stream` already consumes — see [`translate_stream_event`].
/// Usage arrives on the terminal `response.completed` event regardless of
/// `request.stream_include_usage` (the Responses API has no opt-in for it),
/// and is forwarded either way; the caller ignores it when it didn't ask.
pub(crate) async fn complete_stream(request: CompletionRequest<'_>) -> Result<CompletionStream> {
    let client = super::client(request.base_url, request.api_key);
    let cancellation = request.cancellation.clone();
    let body = build_request(&request, true)?;
    trace_request(&body);
    let events = super::await_cancellation(
        client.responses().create_stream_byot::<_, Value>(body),
        cancellation,
    )
    .await?;
    let chunks = events.filter_map(|event| async move {
        match event {
            Ok(event) => translate_stream_event(&event).transpose(),
            Err(error) => Some(Err(error)),
        }
    });
    Ok(Box::pin(chunks))
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<ResponsesText>,
    /// Always `false` — see this module's doc comment on why lait never
    /// uses `previous_response_id`/`conversation` chaining. Explicit
    /// (rather than just omitted, which the API defaults to `true`) so a
    /// user reading a `--trace-file`/`-vv` dump of the request body sees
    /// the intent, not an absence.
    store: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoning {
    effort: &'static str,
}

#[derive(Debug, Serialize)]
struct ResponsesText {
    format: ResponsesTextFormat,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesTextFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

fn build_request(request: &CompletionRequest<'_>, stream: bool) -> Result<ResponsesRequest> {
    let (instructions, input) = to_instructions_and_input(&request.messages)?;
    Ok(ResponsesRequest {
        model: request.model_id.to_owned(),
        input,
        instructions,
        tools: to_tools(request.tools)?,
        reasoning: request.reasoning_effort.map(|effort| ResponsesReasoning {
            effort: effort.as_str(),
        }),
        temperature: request.temperature,
        top_p: request.top_p,
        max_output_tokens: request.max_tokens,
        text: to_text_config(request.response_format.as_ref()),
        store: false,
        stream,
    })
}

/// Flattens Chat Completions tool definitions (`{"type": "function",
/// "function": {"name", "description", "parameters", "strict"}}`) into the
/// Responses API's `{"type": "function", "name", ...}` shape. Goes through
/// JSON rather than matching `ChatCompletionTools`'s variants so a field
/// async-openai adds later (or a non-function tool kind) is carried over or
/// rejected explicitly instead of silently dropped.
fn to_tools(tools: &[ChatCompletionTools]) -> Result<Vec<Value>> {
    tools
        .iter()
        .map(|tool| {
            let value = serde_json::to_value(tool)
                .context("failed to serialize a tool definition for the Responses API")?;
            let Some(Value::Object(function)) = value.get("function") else {
                bail!("only function tools can be sent to the Responses API: {value}");
            };
            let mut flat = serde_json::Map::with_capacity(function.len() + 1);
            flat.insert("type".to_owned(), json!("function"));
            flat.extend(function.clone());
            Ok(Value::Object(flat))
        })
        .collect()
}

/// Splits `messages` into the Responses API's `instructions` (a leading
/// system message, if any — the Responses API has no `system`-role item
/// inside `input`, only this separate top-level field) and `input` (every
/// other message, in order). An assistant message's `tool_calls` become
/// `function_call` items and a `tool`-role message a `function_call_output`
/// item (see this module's doc comment); a later system/developer message
/// (only `lait chat`'s `/system` can produce one mid-history) is sent as a
/// `developer`-role input item rather than dropped.
fn to_instructions_and_input(
    messages: &[ChatCompletionRequestMessage],
) -> Result<(Option<String>, Vec<Value>)> {
    let mut instructions = None;
    let mut input = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let value = serde_json::to_value(message)
            .context("failed to serialize a message for the Responses API")?;
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("a message had no 'role' while building a Responses API request")
            })?
            .to_owned();
        match role.as_str() {
            "system" | "developer" if index == 0 => {
                instructions = Some(extract_text_content(&value)?);
            }
            "system" | "developer" => input.push(json!({
                "role": "developer",
                "content": extract_text_content(&value)?,
            })),
            "user" => input.push(json!({
                "role": "user",
                "content": to_user_content(&value)?,
            })),
            "assistant" => {
                let text = extract_text_content(&value)?;
                let tool_calls = value
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if !text.is_empty() || tool_calls.is_empty() {
                    input.push(json!({"role": "assistant", "content": text}));
                }
                for tool_call in tool_calls {
                    input.push(to_function_call_item(tool_call)?);
                }
            }
            "tool" => {
                let call_id = value
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("a 'tool' message had no 'tool_call_id'"))?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": extract_text_content(&value)?,
                }));
            }
            other => bail!("cannot send a '{other}' message to the Responses API"),
        }
    }
    Ok((instructions, input))
}

/// One Chat Completions `tool_calls[]` entry (`{"id", "type": "function",
/// "function": {"name", "arguments"}}`) as a Responses API `function_call`
/// input item.
fn to_function_call_item(tool_call: &Value) -> Result<Value> {
    let field = |pointer: &str| {
        tool_call
            .pointer(pointer)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("a tool call had no '{pointer}': {tool_call}"))
    };
    Ok(json!({
        "type": "function_call",
        "call_id": field("/id")?,
        "name": field("/function/name")?,
        "arguments": field("/function/arguments")?,
    }))
}

/// A user message's content: a bare string as-is (the shape every
/// attachment-free request uses), or — for an `--image` turn — the content
/// parts translated to `input_text`/`input_image`.
fn to_user_content(message: &Value) -> Result<Value> {
    let Some(Value::Array(parts)) = message.get("content") else {
        return Ok(Value::String(extract_text_content(message)?));
    };
    parts
        .iter()
        .map(|part| match part.get("type").and_then(Value::as_str) {
            Some("text") => Ok(json!({
                "type": "input_text",
                "text": part.get("text").and_then(Value::as_str).unwrap_or_default(),
            })),
            Some("image_url") => {
                let url = part
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("an image content part had no 'image_url.url'"))?;
                Ok(json!({"type": "input_image", "image_url": url}))
            }
            Some("file") => {
                let file = part.get("file").unwrap_or(&Value::Null);
                let mut item = serde_json::Map::new();
                item.insert("type".to_owned(), json!("input_file"));
                for key in ["filename", "file_data", "file_id"] {
                    if let Some(value) = file.get(key) {
                        item.insert(key.to_owned(), value.clone());
                    }
                }
                Ok(Value::Object(item))
            }
            other => bail!("cannot send a {other:?} content part to the Responses API"),
        })
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

/// Extracts plain text from a serialized `ChatCompletionRequestMessage`'s
/// `content` field: a bare string as-is, or an array of content parts
/// (joined) when every part is `{"type": "text", "text": ...}`. Only user
/// messages can carry an image part, and those go through
/// [`to_user_content`] instead, so any other part type here is an error.
fn extract_text_content(message: &Value) -> Result<String> {
    match message.get("content") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                            text.push_str(part_text);
                        }
                    }
                    other => bail!(
                        "cannot send a non-text message content part ({other:?}) here to the \
                         Responses API"
                    ),
                }
            }
            Ok(text)
        }
        Some(other) => bail!("unexpected message content shape: {other}"),
    }
}

fn to_text_config(response_format: Option<&ResponseFormat>) -> Option<ResponsesText> {
    let format = match response_format? {
        ResponseFormat::Text => ResponsesTextFormat::Text,
        ResponseFormat::JsonObject => ResponsesTextFormat::JsonObject,
        ResponseFormat::JsonSchema { json_schema } => ResponsesTextFormat::JsonSchema {
            name: json_schema.name.clone(),
            schema: json_schema.schema.clone(),
            strict: json_schema.strict,
            description: json_schema.description.clone(),
        },
    };
    Some(ResponsesText { format })
}

/// The subset of a Responses API reply lait reads — only the fields it
/// actually consumes are modeled, matching `response::ChatCompletionResponse`'s
/// own "an OpenAI-compatible server's response carries many fields we never
/// read" philosophy. `#[serde(other)]` on `ResponsesOutputItem`/
/// `ResponsesContentPart` skips any output-item/content-part kind besides
/// the text, reasoning summaries, and function calls this module extracts.
#[derive(Debug, Deserialize)]
struct ResponsesApiResponse {
    status: String,
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    error: Option<ResponsesApiError>,
}

#[derive(Debug, Deserialize)]
struct ResponsesApiError {
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesOutputItem {
    Message {
        content: Vec<ResponsesContentPart>,
    },
    Reasoning {
        #[serde(default)]
        summary: Vec<ResponsesContentPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesContentPart {
    OutputText {
        text: String,
    },
    SummaryText {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct ResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

impl ResponsesUsage {
    fn to_chat_usage(&self) -> Value {
        json!({
            "prompt_tokens": self.input_tokens,
            "completion_tokens": self.output_tokens,
            "total_tokens": self.total_tokens,
        })
    }
}

/// Builds the [`response::ChatCompletionResponse`] every other completion
/// path already produces, from a parsed Responses API reply — via
/// `serde_json::from_value` rather than a dedicated constructor in
/// `response.rs`, since that type's fields are deliberately private to that
/// module (see its own doc comment) and its `Deserialize` impl already does
/// exactly the translation this needs. A reasoning item's summary becomes
/// the message's `reasoning` (shown by `--show-reasoning`), and each
/// `function_call` item one entry of `tool_calls`.
fn to_chat_completion_response(
    raw: ResponsesApiResponse,
) -> Result<response::ChatCompletionResponse> {
    if raw.status == "failed" || raw.status == "cancelled" {
        let message = raw
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| format!("Responses API request ended with status '{}'", raw.status));
        bail!("{message}");
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for item in &raw.output {
        match item {
            ResponsesOutputItem::Message { content } => {
                for part in content {
                    if let ResponsesContentPart::OutputText { text: part_text } = part {
                        text.push_str(part_text);
                    }
                }
            }
            ResponsesOutputItem::Reasoning { summary } => {
                for part in summary {
                    if let ResponsesContentPart::SummaryText { text: part_text } = part {
                        if !reasoning.is_empty() {
                            reasoning.push_str("\n\n");
                        }
                        reasoning.push_str(part_text);
                    }
                }
            }
            ResponsesOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => tool_calls.push(json!({
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            })),
            ResponsesOutputItem::Other => {}
        }
    }

    let mut message = json!({"content": text});
    if !reasoning.is_empty() {
        message["reasoning"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let value = json!({
        "choices": [{"message": message}],
        "usage": raw.usage.as_ref().map(ResponsesUsage::to_chat_usage),
    });
    serde_json::from_value(value).context("failed to translate a Responses API reply")
}

/// Translates one Responses API SSE event into the Chat Completions chunk
/// `engine::stream` consumes, or `None` for an event that carries nothing
/// it needs (`response.created`, `...done` events whose content already
/// arrived as deltas, ...). Stateless on purpose: a function call's
/// `output_index` is stable across its `response.output_item.added` (which
/// carries `call_id`/`name`) and every later
/// `response.function_call_arguments.delta`, so it serves directly as the
/// Chat Completions tool-call `index` `response::StreamToolCallAccumulator`
/// reassembles by. A `response.failed`/`error` event becomes a stream error.
fn translate_stream_event(
    event: &Value,
) -> Result<Option<response::ChatCompletionStreamChunk>, OpenAIError> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let delta_text = || {
        event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let chunk = match event_type {
        "response.output_text.delta" => delta_chunk(json!({"content": delta_text()})),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            delta_chunk(json!({"reasoning": delta_text()}))
        }
        "response.output_item.added" => {
            let item = event.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return Ok(None);
            }
            delta_chunk(json!({"tool_calls": [{
                "index": event.get("output_index").and_then(Value::as_u64).unwrap_or_default(),
                "id": item.get("call_id"),
                "function": {
                    "name": item.get("name"),
                    "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or_default(),
                },
            }]}))
        }
        "response.function_call_arguments.delta" => delta_chunk(json!({"tool_calls": [{
            "index": event.get("output_index").and_then(Value::as_u64).unwrap_or_default(),
            "function": {"arguments": delta_text()},
        }]})),
        "response.completed" | "response.incomplete" => {
            let Some(usage) = event.pointer("/response/usage") else {
                return Ok(None);
            };
            let usage: ResponsesUsage = serde_json::from_value(usage.clone())
                .map_err(|error| OpenAIError::JSONDeserialize(error, usage.to_string()))?;
            json!({"choices": [], "usage": usage.to_chat_usage()})
        }
        "response.failed" | "error" => {
            let message = event
                .pointer("/response/error/message")
                .or_else(|| event.get("message"))
                .or_else(|| event.pointer("/error/message"))
                .and_then(Value::as_str)
                .unwrap_or("Responses API stream reported a failure");
            return Err(OpenAIError::StreamError(Box::new(
                StreamError::EventStream(message.to_owned()),
            )));
        }
        _ => return Ok(None),
    };
    serde_json::from_value(chunk.clone())
        .map(Some)
        .map_err(|error| OpenAIError::JSONDeserialize(error, chunk.to_string()))
}

fn delta_chunk(delta: Value) -> Value {
    json!({"choices": [{"delta": delta}]})
}

/// Dumps `body` as JSON at trace level (`-vv`/`LAIT_LOG=trace`) — mirrors
/// `super::trace_request`'s Chat Completions counterpart. Safe to log whole
/// for the same reason: the request body never carries `api_key`.
fn trace_request(body: &ResponsesRequest) {
    if !tracing::event_enabled!(tracing::Level::TRACE) {
        return;
    }
    match serde_json::to_string(body) {
        Ok(json) => tracing::trace!(request = %json, "sending Responses API request"),
        Err(error) => {
            tracing::trace!(error = %error, "failed to serialize Responses API request for tracing")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        extract_text_content, to_chat_completion_response, to_instructions_and_input,
        translate_stream_event,
    };
    use crate::llm::{
        assistant_message, assistant_tool_call_message, initial_messages, tool_result_message,
    };
    use crate::response::{self, ToolCall, ToolCallFunction};
    use serde_json::json;

    #[test]
    fn splits_a_leading_system_message_into_instructions() {
        let messages = initial_messages(Some("be terse"), &[], "hello", &[]).unwrap();
        let (instructions, input) = to_instructions_and_input(&messages).unwrap();
        assert_eq!(instructions.as_deref(), Some("be terse"));
        assert_eq!(input, vec![json!({"role": "user", "content": "hello"})]);
    }

    #[test]
    fn works_without_a_leading_system_message() {
        let messages = initial_messages(None, &[], "hello", &[]).unwrap();
        let (instructions, input) = to_instructions_and_input(&messages).unwrap();
        assert_eq!(instructions, None);
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn preserves_multi_turn_history_order() {
        let mut messages = initial_messages(Some("sys"), &[], "first", &[]).unwrap();
        messages.push(assistant_message("first reply").unwrap());
        messages.push(crate::llm::user_message("second", &[]).unwrap());
        let (instructions, input) = to_instructions_and_input(&messages).unwrap();
        assert_eq!(instructions.as_deref(), Some("sys"));
        assert_eq!(
            input,
            vec![
                json!({"role": "user", "content": "first"}),
                json!({"role": "assistant", "content": "first reply"}),
                json!({"role": "user", "content": "second"}),
            ]
        );
    }

    #[test]
    fn translates_tool_calls_and_tool_results_into_function_call_items() {
        let mut messages = initial_messages(None, &[], "what time is it?", &[]).unwrap();
        let tool_calls = [ToolCall {
            id: "call_1".to_owned(),
            function: ToolCallFunction {
                name: "clock__now".to_owned(),
                arguments: "{}".to_owned(),
            },
        }];
        messages.push(assistant_tool_call_message(&tool_calls, Some("checking")).unwrap());
        messages.push(tool_result_message("call_1", "12:00".to_owned()).unwrap());
        let (_, input) = to_instructions_and_input(&messages).unwrap();
        assert_eq!(
            input,
            vec![
                json!({"role": "user", "content": "what time is it?"}),
                json!({"role": "assistant", "content": "checking"}),
                json!({"type": "function_call", "call_id": "call_1", "name": "clock__now", "arguments": "{}"}),
                json!({"type": "function_call_output", "call_id": "call_1", "output": "12:00"}),
            ]
        );
    }

    #[test]
    fn a_tool_call_turn_without_text_sends_no_empty_assistant_message() {
        let tool_calls = [ToolCall {
            id: "call_1".to_owned(),
            function: ToolCallFunction {
                name: "t".to_owned(),
                arguments: "{}".to_owned(),
            },
        }];
        let messages = vec![assistant_tool_call_message(&tool_calls, None).unwrap()];
        let (_, input) = to_instructions_and_input(&messages).unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call");
    }

    #[test]
    fn translates_image_attachments_into_input_image_parts() {
        let media = [crate::attachment::MediaPart::Image(
            "https://example.com/x.png".to_owned(),
        )];
        let messages = initial_messages(None, &[], "describe", &media).unwrap();
        let (_, input) = to_instructions_and_input(&messages).unwrap();
        assert_eq!(
            input,
            vec![json!({"role": "user", "content": [
                {"type": "input_text", "text": "describe"},
                {"type": "input_image", "image_url": "https://example.com/x.png"},
            ]})]
        );
    }

    #[test]
    fn translates_pdf_attachments_into_input_file_parts() {
        let media = [crate::attachment::MediaPart::File {
            filename: "a.pdf".to_owned(),
            data_url: "data:application/pdf;base64,JVBERi0=".to_owned(),
        }];
        let messages = initial_messages(None, &[], "summarize", &media).unwrap();
        let (_, input) = to_instructions_and_input(&messages).unwrap();
        assert_eq!(
            input[0]["content"][1],
            json!({"type": "input_file", "filename": "a.pdf", "file_data": "data:application/pdf;base64,JVBERi0="})
        );
    }

    #[test]
    fn extract_text_content_reads_a_plain_string() {
        let value = json!({"content": "hi"});
        assert_eq!(extract_text_content(&value).unwrap(), "hi");
    }

    #[test]
    fn extract_text_content_joins_text_parts() {
        let value = json!({"content": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b"},
        ]});
        assert_eq!(extract_text_content(&value).unwrap(), "ab");
    }

    #[test]
    fn extract_text_content_rejects_an_image_part() {
        let value = json!({"content": [
            {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}},
        ]});
        assert!(extract_text_content(&value).is_err());
    }

    #[test]
    fn to_chat_completion_response_extracts_message_text_and_usage() {
        let raw = serde_json::from_value(json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "hello there"}
                ]},
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
        }))
        .unwrap();
        let response = to_chat_completion_response(raw).unwrap();
        assert_eq!(response::content_text(&response), "hello there");
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn to_chat_completion_response_extracts_function_calls_and_reasoning() {
        let raw = serde_json::from_value(json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "need the clock"}]},
                {"type": "function_call", "call_id": "call_9", "name": "clock__now", "arguments": "{\"tz\":\"UTC\"}"},
            ],
        }))
        .unwrap();
        let response = to_chat_completion_response(raw).unwrap();
        assert_eq!(
            response::response_reasoning(&response),
            Some("need the clock")
        );
        let message = response::first_message(&response).unwrap();
        let tool_calls = message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_9");
        assert_eq!(tool_calls[0].function.name, "clock__now");
        assert_eq!(tool_calls[0].function.arguments, r#"{"tz":"UTC"}"#);
    }

    #[test]
    fn to_chat_completion_response_errors_on_a_failed_status() {
        let raw = serde_json::from_value(json!({
            "status": "failed",
            "output": [],
            "error": {"message": "insufficient_quota"},
        }))
        .unwrap();
        let error = to_chat_completion_response(raw).unwrap_err();
        assert!(error.to_string().contains("insufficient_quota"));
    }

    #[test]
    fn stream_events_translate_into_content_and_tool_call_chunks() {
        let text =
            translate_stream_event(&json!({"type": "response.output_text.delta", "delta": "Hel"}))
                .unwrap()
                .unwrap();
        assert_eq!(response::stream_chunk_deltas(&text), (Some("Hel"), None));

        let mut accumulator = response::StreamToolCallAccumulator::default();
        for event in [
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "call_id": "call_1", "name": "t", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"a\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "1}"}),
        ] {
            let chunk = translate_stream_event(&event).unwrap().unwrap();
            accumulator.push(response::stream_chunk_tool_call_deltas(&chunk).unwrap());
        }
        let calls = accumulator.finish().unwrap();
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "t");
        assert_eq!(calls[0].function.arguments, r#"{"a":1}"#);
    }

    #[test]
    fn stream_completed_event_carries_usage_and_irrelevant_events_are_skipped() {
        let chunk = translate_stream_event(&json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}},
        }))
        .unwrap()
        .unwrap();
        assert_eq!(chunk.usage.unwrap().total_tokens, 5);
        assert!(
            translate_stream_event(&json!({"type": "response.created"}))
                .unwrap()
                .is_none()
        );
        assert!(
            translate_stream_event(
                &json!({"type": "response.output_item.added", "item": {"type": "message"}})
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn stream_failure_events_become_errors() {
        let error = translate_stream_event(&json!({
            "type": "response.failed",
            "response": {"error": {"message": "rate_limited"}},
        }))
        .unwrap_err();
        assert!(error.to_string().contains("rate_limited"), "{error}");
    }
}
