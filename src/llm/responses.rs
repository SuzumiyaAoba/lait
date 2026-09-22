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
//! # What this does not do (v1)
//!
//! `engine::transport::RequestSettings::complete`/`complete_stream` refuse a
//! Responses-API model outright when `mcp:`/`subagents:`/`tools:` name
//! anything, or an `--image` attachment is present, or streaming is
//! requested — this module is therefore only ever asked to translate plain
//! System/User/Assistant text messages, never a `tool`-role message, a
//! multipart (image) content array, or a `stream: true` request. See
//! `docs/usage/ja/config.md`'s Responses API section for the full list and
//! the reasoning behind each cut. `store` is always sent as `false` (no
//! server-side retention) and `previous_response_id`/`conversation` are
//! never used — every round resends the full message history, exactly like
//! Chat Completions, specifically so the disk cache/`--record`/`--replay`
//! keep working unmodified (see `cache::key`'s own doc comment, which this
//! module does not change).

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ChatCompletionRequestMessage, ResponseFormat};
use serde::{Deserialize, Serialize};

use crate::response;

use super::CompletionRequest;

pub(crate) async fn complete(
    request: CompletionRequest<'_>,
) -> Result<response::ChatCompletionResponse> {
    let client = super::client(request.base_url, request.api_key);
    let cancellation = request.cancellation.clone();
    let body = build_request(&request)?;
    trace_request(&body);
    let raw: ResponsesApiResponse =
        super::await_cancellation(client.responses().create_byot(body), cancellation).await?;
    tracing::trace!(response = ?raw, "received responses API response");
    to_chat_completion_response(raw)
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
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
}

#[derive(Debug, Serialize)]
struct ResponsesInputItem {
    role: &'static str,
    content: String,
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

fn build_request(request: &CompletionRequest<'_>) -> Result<ResponsesRequest> {
    let (instructions, input) = to_instructions_and_input(&request.messages)?;
    Ok(ResponsesRequest {
        model: request.model_id.to_owned(),
        input,
        instructions,
        reasoning: request.reasoning_effort.map(|effort| ResponsesReasoning {
            effort: effort.as_str(),
        }),
        temperature: request.temperature,
        top_p: request.top_p,
        max_output_tokens: request.max_tokens,
        text: to_text_config(request.response_format.as_ref()),
        store: false,
    })
}

/// Splits `messages` into the Responses API's `instructions` (a leading
/// system message, if any — the Responses API has no `system`-role item
/// inside `input`, only this separate top-level field) and `input` (every
/// other message, in order). Only ever sees System/User/Assistant messages
/// with plain-text content — `engine::transport` refuses `mcp:`/
/// `subagents:`/`tools:` and `--image` for a Responses-API model before a
/// `tool`-role message or an image content part could ever reach here (see
/// this module's doc comment) — so a message this function can't translate
/// is a genuine internal-invariant violation, not a normal user error, and
/// errors accordingly rather than silently dropping content.
fn to_instructions_and_input(
    messages: &[ChatCompletionRequestMessage],
) -> Result<(Option<String>, Vec<ResponsesInputItem>)> {
    let mut instructions = None;
    let mut input = Vec::with_capacity(messages.len());
    for message in messages {
        let value = serde_json::to_value(message)
            .context("failed to serialize a message for the Responses API")?;
        let role = value
            .get("role")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow!("a message had no 'role' while building a Responses API request")
            })?
            .to_owned();
        let text = extract_text_content(&value)?;
        match role.as_str() {
            "system" | "developer" if instructions.is_none() => instructions = Some(text),
            "user" => input.push(ResponsesInputItem {
                role: "user",
                content: text,
            }),
            "assistant" => input.push(ResponsesInputItem {
                role: "assistant",
                content: text,
            }),
            other => bail!(
                "internal error: a Responses API model was asked to send a '{other}' message; \
                 this should have been rejected before reaching the request builder"
            ),
        }
    }
    Ok((instructions, input))
}

/// Extracts plain text from a serialized `ChatCompletionRequestMessage`'s
/// `content` field: a bare string as-is, or an array of content parts
/// (joined) when every part is `{"type": "text", "text": ...}` — any other
/// part type (`image_url`, ...) is rejected, since `engine::transport`'s
/// upstream guard means one should never actually reach here for a
/// Responses-API model (see this module's doc comment).
fn extract_text_content(message: &serde_json::Value) -> Result<String> {
    match message.get("content") {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(serde_json::Value::String(text)) => Ok(text.clone()),
        Some(serde_json::Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                match part.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => {
                        if let Some(part_text) =
                            part.get("text").and_then(serde_json::Value::as_str)
                        {
                            text.push_str(part_text);
                        }
                    }
                    other => bail!(
                        "internal error: a Responses API model was asked to send a \
                         non-text message content part ({other:?}); this should have been \
                         rejected before reaching the request builder"
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
/// the plain text this module extracts (`reasoning`, `function_call`, ... —
/// none of which v1 produces, since tool calling is refused upstream, but a
/// reasoning model always emits its own `reasoning` output items regardless
/// of whether tools are in play).
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
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesContentPart {
    OutputText {
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

/// Builds the [`response::ChatCompletionResponse`] every other completion
/// path already produces, from a parsed Responses API reply — via
/// `serde_json::from_value` rather than a dedicated constructor in
/// `response.rs`, since that type's fields are deliberately private to that
/// module (see its own doc comment) and its `Deserialize` impl already does
/// exactly the translation this needs.
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
    for item in &raw.output {
        let ResponsesOutputItem::Message { content } = item else {
            continue;
        };
        for part in content {
            if let ResponsesContentPart::OutputText { text: part_text } = part {
                text.push_str(part_text);
            }
        }
    }

    let usage = raw.usage.map(|usage| {
        serde_json::json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens,
        })
    });
    let value = serde_json::json!({
        "choices": [{"message": {"content": text}}],
        "usage": usage,
    });
    serde_json::from_value(value).context("failed to translate a Responses API reply")
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
    use super::{extract_text_content, to_chat_completion_response, to_instructions_and_input};
    use crate::llm::{assistant_message, initial_messages};

    #[test]
    fn splits_a_leading_system_message_into_instructions() {
        let messages = initial_messages(Some("be terse"), &[], "hello", &[]).unwrap();
        let (instructions, input) = to_instructions_and_input(&messages).unwrap();
        assert_eq!(instructions.as_deref(), Some("be terse"));
        assert_eq!(input.len(), 1);
        assert_eq!(input[0].role, "user");
        assert_eq!(input[0].content, "hello");
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
        let roles: Vec<&str> = input.iter().map(|item| item.role).collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
        assert_eq!(input[0].content, "first");
        assert_eq!(input[1].content, "first reply");
        assert_eq!(input[2].content, "second");
    }

    #[test]
    fn extract_text_content_reads_a_plain_string() {
        let value = serde_json::json!({"content": "hi"});
        assert_eq!(extract_text_content(&value).unwrap(), "hi");
    }

    #[test]
    fn extract_text_content_joins_text_parts() {
        let value = serde_json::json!({"content": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b"},
        ]});
        assert_eq!(extract_text_content(&value).unwrap(), "ab");
    }

    #[test]
    fn extract_text_content_rejects_an_image_part() {
        let value = serde_json::json!({"content": [
            {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}},
        ]});
        assert!(extract_text_content(&value).is_err());
    }

    #[test]
    fn to_chat_completion_response_extracts_message_text_and_usage() {
        let raw = serde_json::from_value(serde_json::json!({
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
        assert_eq!(crate::response::content_text(&response), "hello there");
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn to_chat_completion_response_errors_on_a_failed_status() {
        let raw = serde_json::from_value(serde_json::json!({
            "status": "failed",
            "output": [],
            "error": {"message": "insufficient_quota"},
        }))
        .unwrap();
        let error = to_chat_completion_response(raw).unwrap_err();
        assert!(error.to_string().contains("insufficient_quota"));
    }
}
