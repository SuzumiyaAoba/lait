use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionResponse {
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

/// A single `chat.completion.chunk` from a streamed (`stream: true`)
/// response, deserialized the same way `ChatCompletionResponse` is: only the
/// fields `--stream` needs to render, tolerant of whatever else a given
/// server includes.
#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionStreamChunk {
    #[serde(default)]
    choices: Vec<ChatCompletionStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionStreamChoice {
    delta: ChatCompletionStreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct ChatCompletionStreamDelta {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
}

/// The content-delta and reasoning-delta text carried by a chunk's first
/// choice (chat completions only ever stream one choice for a single-turn
/// request), applying the same current-field/legacy-`reasoning_content`
/// fallback as `response_reasoning`. Either can be `None`: a chunk may carry
/// no choices at all (e.g. the final `usage`-only chunk), and most chunks set
/// only one of `content`/`reasoning` (e.g. the first chunk sets only `role`).
pub(crate) fn stream_chunk_deltas(
    chunk: &ChatCompletionStreamChunk,
) -> (Option<&str>, Option<&str>) {
    let Some(choice) = chunk.choices.first() else {
        return (None, None);
    };
    let content = choice
        .delta
        .content
        .as_deref()
        .filter(|text| !text.is_empty());
    let reasoning = choice
        .delta
        .reasoning
        .as_deref()
        .filter(|text| !text.is_empty())
        .or_else(|| {
            choice
                .delta
                .reasoning_content
                .as_deref()
                .filter(|text| !text.is_empty())
        });
    (content, reasoning)
}

pub(crate) fn render_response(
    response: &ChatCompletionResponse,
    as_json: bool,
    show_reasoning: bool,
) -> Result<String> {
    let content = response_content(response).map_err(anyhow::Error::msg)?;
    let reasoning = response_reasoning(response);

    if as_json {
        Ok(serde_json::to_string(&JsonOutput { content, reasoning })?)
    } else {
        Ok(format_response(content, reasoning, show_reasoning))
    }
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
        ChatCompletionResponse, ChatCompletionStreamChunk, format_response, response_content,
        response_reasoning, stream_chunk_deltas,
    };

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

    #[test]
    fn extracts_content_and_reasoning_deltas_from_a_stream_chunk() {
        let chunk = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"content": "Hel", "reasoning": "thinking"}}]
        }))
        .expect("chunk fixture should deserialize");
        assert_eq!(stream_chunk_deltas(&chunk), (Some("Hel"), Some("thinking")));
    }

    #[test]
    fn treats_a_role_only_or_empty_delta_as_no_deltas() {
        let role_only = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"role": "assistant"}}]
        }))
        .expect("chunk fixture should deserialize");
        assert_eq!(stream_chunk_deltas(&role_only), (None, None));

        let empty_strings =
            serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
                "choices": [{"delta": {"content": "", "reasoning": ""}}]
            }))
            .expect("chunk fixture should deserialize");
        assert_eq!(stream_chunk_deltas(&empty_strings), (None, None));
    }

    #[test]
    fn treats_a_choiceless_chunk_as_no_deltas() {
        let chunk = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": []
        }))
        .expect("chunk fixture should deserialize");
        assert_eq!(stream_chunk_deltas(&chunk), (None, None));
    }

    #[test]
    fn falls_back_to_legacy_reasoning_content_delta() {
        let chunk = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"reasoning_content": "legacy delta"}}]
        }))
        .expect("chunk fixture should deserialize");
        assert_eq!(stream_chunk_deltas(&chunk), (None, Some("legacy delta")));
    }
}
