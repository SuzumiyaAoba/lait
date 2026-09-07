use serde::{Deserialize, Serialize};

mod render;
mod stream;

pub(crate) use render::{render_response, render_text_json};
pub(crate) use stream::{
    ChatCompletionStreamChunk, StreamToolCallAccumulator, stream_chunk_deltas,
    stream_chunk_tool_call_deltas,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
    /// The token counts the server reported for this request, when it did —
    /// OpenAI-compatible servers are not obliged to, so every consumer
    /// treats `None` as "not reported" rather than zero.
    #[serde(default)]
    pub(crate) usage: Option<Usage>,
}

/// The `usage` object of a chat completion response (or of a streamed
/// response's final chunk, when `stream_options: {"include_usage": true}`
/// was requested). Fields default to 0 individually so a server reporting
/// only some of them still parses.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) prompt_tokens: u64,
    #[serde(default)]
    pub(crate) completion_tokens: u64,
    #[serde(default)]
    pub(crate) total_tokens: u64,
}

impl Usage {
    /// Accumulates `other` into `self`, for summing usage across a tool
    /// loop's rounds or a workflow's steps.
    pub(crate) fn add(&mut self, other: Usage) {
        // Usage is reported by the remote server, so it is untrusted input.
        // Saturating here keeps a malicious or simply overflowing aggregate
        // from panicking in debug builds (or wrapping in release builds) and
        // preserves the useful upper-bound information we already have.
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "prompt={} completion={} total={}",
            self.prompt_tokens, self.completion_tokens, self.total_tokens
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ChatCompletionChoice {
    message: ChatCompletionResponseMessage,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ChatCompletionResponseMessage {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    /// The tool calls the model asked to make, when it stopped to call a
    /// tool instead of (or in addition to) producing `content`. `None`/empty
    /// when the server doesn't support tool calling or the model didn't call
    /// one this turn.
    #[serde(default)]
    pub(crate) tool_calls: Option<Vec<ToolCall>>,
}

/// One tool call from a `tool_calls` response field, in the OpenAI chat
/// completions shape (`{"id", "type": "function", "function": {"name",
/// "arguments"}}`). `arguments` is the raw JSON text the model produced, not
/// yet parsed — parsing/validating it is `McpRegistry::call`'s job.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) function: ToolCallFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ToolCallFunction {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

/// The first choice's message, for callers (the tool loop in `app.rs`) that
/// need to inspect `tool_calls`/raw `content` before deciding whether a lack
/// of `content` is even an error — unlike `response_content`, this never
/// errors on an empty/missing `content`.
pub(crate) fn first_message(
    response: &ChatCompletionResponse,
) -> Option<&ChatCompletionResponseMessage> {
    response.choices.first().map(|choice| &choice.message)
}

/// The first choice's raw content text, or `""` when there is none — the
/// shape a `--session`/`lait history` turn record needs (unlike
/// `response_content`, never an error; unlike `render_response`, never
/// `Reasoning:`-prefixed).
pub(crate) fn content_text(response: &ChatCompletionResponse) -> &str {
    first_message(response)
        .and_then(|message| message.content())
        .unwrap_or_default()
}

impl ChatCompletionResponseMessage {
    pub(crate) fn content(&self) -> Option<&str> {
        self.content
            .as_deref()
            .filter(|content| !content.is_empty())
    }
}

fn response_content(response: &ChatCompletionResponse) -> std::result::Result<&str, &'static str> {
    let message = first_message(response).ok_or("API response contained no choices")?;
    message
        .content()
        .ok_or("API response contained no content in its first choice")
}

/// The first choice's non-blank reasoning text, preferring the current
/// `reasoning` field over the legacy `reasoning_content`. Exposed for the
/// chat `-o` path, which sends reasoning to stderr while the file gets the
/// body alone.
pub(crate) fn response_reasoning(response: &ChatCompletionResponse) -> Option<&str> {
    let choice = response.choices.first()?;
    non_blank_or(
        choice.message.reasoning.as_deref(),
        choice.message.reasoning_content.as_deref(),
        |reasoning| !reasoning.trim().is_empty(),
    )
}

/// `current` if it passes `is_present`, else `legacy` if it passes
/// `is_present` — the shared current-field/legacy-`reasoning_content`
/// fallback used by both [`response_reasoning`] and [`stream_chunk_deltas`].
/// Takes the presence predicate rather than hardcoding it: the full response
/// treats whitespace-only reasoning as absent (`.trim().is_empty()`), while a
/// stream delta only treats a literal empty string as absent, since a chunk
/// may legitimately carry a single space as part of a larger reasoning run.
fn non_blank_or<'a>(
    current: Option<&'a str>,
    legacy: Option<&'a str>,
    is_present: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    current
        .filter(|text| is_present(text))
        .or_else(|| legacy.filter(|text| is_present(text)))
}

#[cfg(test)]
mod tests {
    use super::render::format_response;
    use super::{
        ChatCompletionResponse, ChatCompletionStreamChunk, StreamToolCallAccumulator, Usage,
        response_content, response_reasoning, stream_chunk_deltas, stream_chunk_tool_call_deltas,
    };

    #[test]
    fn usage_add_saturates_untrusted_token_counts() {
        let mut usage = Usage {
            prompt_tokens: u64::MAX,
            completion_tokens: u64::MAX - 1,
            total_tokens: 1,
        };
        usage.add(Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: u64::MAX,
        });

        assert_eq!(
            usage,
            Usage {
                prompt_tokens: u64::MAX,
                completion_tokens: u64::MAX,
                total_tokens: u64::MAX,
            }
        );
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

    #[test]
    fn extracts_tool_call_deltas_from_a_stream_chunk() {
        let chunk = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "echo", "arguments": ""}}
            ]}}]
        }))
        .expect("chunk fixture should deserialize");
        let deltas = stream_chunk_tool_call_deltas(&chunk).expect("expected tool call deltas");
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].index, 0);
        assert_eq!(deltas[0].id.as_deref(), Some("call_1"));
    }

    #[test]
    fn a_chunk_with_no_tool_calls_has_no_tool_call_deltas() {
        let chunk = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"content": "hi"}}]
        }))
        .expect("chunk fixture should deserialize");
        assert!(stream_chunk_tool_call_deltas(&chunk).is_none());
    }

    #[test]
    fn accumulator_reassembles_arguments_split_across_many_fragments() {
        let mut accumulator = StreamToolCallAccumulator::default();
        assert!(accumulator.is_empty());

        let first = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "echo", "arguments": "{\"te"}}
            ]}}]
        }))
        .expect("chunk fixture should deserialize");
        accumulator.push(stream_chunk_tool_call_deltas(&first).unwrap());
        assert!(!accumulator.is_empty());

        let second = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "xt\":\"hi\"}"}}
            ]}}]
        }))
        .expect("chunk fixture should deserialize");
        accumulator.push(stream_chunk_tool_call_deltas(&second).unwrap());

        let calls = accumulator.finish().expect("expected a complete tool call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "echo");
        assert_eq!(calls[0].function.arguments, r#"{"text":"hi"}"#);
    }

    #[test]
    fn accumulator_reassembles_interleaved_calls_in_index_order() {
        let mut accumulator = StreamToolCallAccumulator::default();
        let chunk = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 1, "id": "call_b", "function": {"name": "second", "arguments": ""}},
                {"index": 0, "id": "call_a", "function": {"name": "first", "arguments": ""}}
            ]}}]
        }))
        .expect("chunk fixture should deserialize");
        accumulator.push(stream_chunk_tool_call_deltas(&chunk).unwrap());

        let calls = accumulator
            .finish()
            .expect("expected two complete tool calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[1].id, "call_b");
    }

    #[test]
    fn accumulator_errors_on_a_call_that_never_received_a_name() {
        let mut accumulator = StreamToolCallAccumulator::default();
        let chunk = serde_json::from_value::<ChatCompletionStreamChunk>(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"arguments": "{}"}}
            ]}}]
        }))
        .expect("chunk fixture should deserialize");
        accumulator.push(stream_chunk_tool_call_deltas(&chunk).unwrap());
        assert!(accumulator.finish().is_err());
    }
}
