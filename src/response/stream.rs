//! Wire types and state for streamed chat-completion responses.
//!
//! Streaming has a slightly different payload from a completed response:
//! content arrives as deltas and tool calls arrive as index-keyed fragments.
//! Keeping those protocol details together leaves the response rendering
//! module concerned only with completed responses and presentation.

use anyhow::{Result, anyhow};
use serde::Deserialize;

use super::{ToolCall, ToolCallFunction, non_blank_or};

/// A single `chat.completion.chunk` from a streamed (`stream: true`)
/// response. Only the fields consumed by the CLI are represented, so
/// OpenAI-compatible servers may include additional fields freely.
#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionStreamChunk {
    #[serde(default)]
    choices: Vec<ChatCompletionStreamChoice>,
    /// Set only on the final, choiceless chunk when usage was requested.
    #[serde(default)]
    pub(crate) usage: Option<super::Usage>,
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
    /// Index-keyed tool-call fragments. A chunk can interleave fragments for
    /// several calls or omit an index that was not updated in that chunk.
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

/// One fragment of a streamed tool call. `id` and function `name` are
/// normally present only on the first fragment; arguments are concatenated in
/// arrival order by [`StreamToolCallAccumulator`].
#[derive(Debug, Deserialize)]
pub(crate) struct StreamToolCallDelta {
    pub(crate) index: usize,
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) function: Option<StreamToolCallFunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamToolCallFunctionDelta {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) arguments: Option<String>,
}

/// Reassembles index-keyed fragments into the same [`ToolCall`] shape used by
/// a non-streamed response, so both transports share tool dispatch.
#[derive(Debug, Default)]
pub(crate) struct StreamToolCallAccumulator {
    by_index: std::collections::BTreeMap<usize, PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl StreamToolCallAccumulator {
    /// Folds one chunk's tool-call fragments into the accumulator.
    pub(crate) fn push(&mut self, deltas: &[StreamToolCallDelta]) {
        for delta in deltas {
            let partial = self.by_index.entry(delta.index).or_default();
            if let Some(id) = &delta.id {
                partial.id = Some(id.clone());
            }
            if let Some(function) = &delta.function {
                if let Some(name) = &function.name {
                    partial.name = Some(name.clone());
                }
                if let Some(arguments) = &function.arguments {
                    partial.arguments.push_str(arguments);
                }
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_index.is_empty()
    }

    /// Reconstructs calls in index order and rejects truncated fragments that
    /// never supplied an id or function name.
    pub(crate) fn finish(self) -> Result<Vec<ToolCall>> {
        self.by_index
            .into_values()
            .map(|partial| {
                let id = partial
                    .id
                    .ok_or_else(|| anyhow!("a streamed tool call never received an 'id'"))?;
                let name = partial.name.ok_or_else(|| {
                    anyhow!("a streamed tool call never received a function 'name'")
                })?;
                Ok(ToolCall {
                    id,
                    function: ToolCallFunction {
                        name,
                        arguments: partial.arguments,
                    },
                })
            })
            .collect()
    }
}

/// Returns the first choice's content and reasoning deltas. A choiceless
/// usage-only chunk yields `(None, None)`.
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
    let reasoning = non_blank_or(
        choice.delta.reasoning.as_deref(),
        choice.delta.reasoning_content.as_deref(),
        |text| !text.is_empty(),
    );
    (content, reasoning)
}

/// Returns the first choice's streamed tool-call fragments, if any.
pub(crate) fn stream_chunk_tool_call_deltas(
    chunk: &ChatCompletionStreamChunk,
) -> Option<&[StreamToolCallDelta]> {
    chunk.choices.first()?.delta.tool_calls.as_deref()
}
