//! Stream consumption and output boundaries for completion responses.
//!
//! The model-facing stream is independent from where content is presented.
//! `stream_response_to` therefore accepts writers supplied by its caller,
//! making cancellation, reasoning formatting, and tool-call accumulation
//! testable without replacing process stdout or creating temporary files.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::path::Path;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::{async_io, error::Interrupted, llm, response};

/// What one streamed model round produced.
#[derive(Debug)]
pub(crate) struct StreamOutcome {
    pub(crate) content: String,
    pub(crate) tool_calls: Vec<response::ToolCall>,
    pub(crate) usage: Option<response::Usage>,
}

/// Consumes a stream into caller-provided writers.
///
/// `reasoning_sink == None` means reasoning is part of the content output and
/// receives the `Reasoning:` header. With a separate sink (the `-o` case),
/// reasoning is written there while content remains clean. Keeping these
/// writers explicit avoids hard-coded global stdout/stderr in the protocol
/// logic and allows unit tests to capture both outputs.
pub(super) async fn stream_response_to<C, R>(
    mut stream: llm::CompletionStream,
    show_reasoning: bool,
    content_sink: &mut C,
    mut reasoning_sink: Option<&mut R>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<StreamOutcome>
where
    C: AsyncWrite + Unpin,
    R: AsyncWrite + Unpin,
{
    let reasoning_inline = reasoning_sink.is_none();
    let mut wrote_reasoning = false;
    let mut wrote_content = false;
    let mut last_usage = None;
    let mut content_text = String::new();
    let mut tool_calls = response::StreamToolCallAccumulator::default();

    loop {
        let chunk = match async_io::await_cancellation(stream.next(), cancellation.clone()).await {
            async_io::CancellationResult::Cancelled => {
                flush_outputs(content_sink, reasoning_sink.as_deref_mut()).await?;
                bail!(Interrupted::cancelled("streamed completion was cancelled"));
            }
            async_io::CancellationResult::Completed(None) => break,
            async_io::CancellationResult::Completed(Some(chunk)) => chunk?,
        };
        tracing::trace!(chunk = ?chunk, "received stream chunk");
        if let Some(usage) = chunk.usage {
            last_usage = Some(usage);
        }
        if let Some(deltas) = response::stream_chunk_tool_call_deltas(&chunk) {
            tool_calls.push(deltas);
        }
        let (content, reasoning) = response::stream_chunk_deltas(&chunk);
        if show_reasoning && let Some(reasoning) = reasoning {
            if reasoning_inline {
                if !wrote_reasoning {
                    content_sink.write_all(b"Reasoning:\n").await?;
                }
                content_sink.write_all(reasoning.as_bytes()).await?;
                content_sink.flush().await?;
            } else if let Some(reasoning_sink) = reasoning_sink.as_deref_mut() {
                if !wrote_reasoning {
                    reasoning_sink.write_all(b"Reasoning:\n").await?;
                }
                reasoning_sink.write_all(reasoning.as_bytes()).await?;
                reasoning_sink.flush().await?;
            }
            wrote_reasoning = true;
        }
        if let Some(content) = content {
            if reasoning_inline && wrote_reasoning && !wrote_content {
                content_sink.write_all(b"\n\n").await?;
            }
            content_sink.write_all(content.as_bytes()).await?;
            // Stdout is live output, while the file path uses a BufWriter at
            // the caller and is flushed once at the end of the round.
            if reasoning_inline {
                content_sink.flush().await?;
            }
            wrote_content = true;
            content_text.push_str(content);
        }
    }

    if !wrote_content && tool_calls.is_empty() {
        bail!("API response contained no content in its first choice");
    }
    if !reasoning_inline
        && wrote_reasoning
        && let Some(reasoning_sink) = reasoning_sink.as_deref_mut()
    {
        reasoning_sink.write_all(b"\n").await?;
        reasoning_sink.flush().await?;
    }
    // A tool round is an intermediate presentation, so it intentionally has
    // no separator/newline. The next round continues the same visible answer.
    if wrote_content && tool_calls.is_empty() {
        content_sink.write_all(b"\n").await?;
    }
    flush_outputs(content_sink, reasoning_sink).await?;
    Ok(StreamOutcome {
        content: content_text,
        usage: last_usage,
        tool_calls: tool_calls.finish()?,
    })
}

async fn flush_outputs<C, R>(content_sink: &mut C, reasoning_sink: Option<&mut R>) -> Result<()>
where
    C: AsyncWrite + Unpin,
    R: AsyncWrite + Unpin,
{
    content_sink.flush().await?;
    if let Some(reasoning_sink) = reasoning_sink {
        reasoning_sink.flush().await?;
    }
    Ok(())
}

/// Process-facing adapter: opens stdout or the requested output file and
/// delegates stream protocol handling to [`stream_response_to`].
pub(super) async fn stream_response(
    stream: llm::CompletionStream,
    show_reasoning: bool,
    output_path: Option<&Path>,
    append: bool,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<StreamOutcome> {
    let mut stdout_writer;
    let mut file_writer;
    let mut stderr_writer;
    match output_path {
        None => {
            stdout_writer = tokio::io::stdout();
            stream_response_to(
                stream,
                show_reasoning,
                &mut stdout_writer,
                None::<&mut tokio::io::Stderr>,
                cancellation,
            )
            .await
        }
        Some(path) => {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(path)
                .await
                .with_context(|| format!("failed to create output file '{}'", path.display()))?;
            file_writer = tokio::io::BufWriter::new(file);
            stderr_writer = tokio::io::stderr();
            stream_response_to(
                stream,
                show_reasoning,
                &mut file_writer,
                Some(&mut stderr_writer),
                cancellation,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::error::OpenAIError;
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    #[derive(Default, Debug)]
    struct CaptureWriter {
        bytes: Vec<u8>,
    }

    impl AsyncWrite for CaptureWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.bytes.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn chunk(value: serde_json::Value) -> response::ChatCompletionStreamChunk {
        serde_json::from_value(value).expect("stream fixture should deserialize")
    }

    fn stream(chunks: Vec<response::ChatCompletionStreamChunk>) -> llm::CompletionStream {
        Box::pin(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, OpenAIError>),
        ))
    }

    #[tokio::test]
    async fn writes_inline_reasoning_and_content_to_the_injected_sink() {
        let mut content = CaptureWriter::default();
        let outcome = stream_response_to(
            stream(vec![chunk(serde_json::json!({
                "choices": [{"delta": {"reasoning": "think", "content": "answer"}}]
            }))]),
            true,
            &mut content,
            None::<&mut CaptureWriter>,
            None,
        )
        .await
        .expect("stream should render");

        assert_eq!(outcome.content, "answer");
        assert_eq!(
            String::from_utf8(content.bytes).unwrap(),
            "Reasoning:\nthink\n\nanswer\n"
        );
    }

    #[tokio::test]
    async fn keeps_reasoning_out_of_the_content_sink_when_separate_output_is_injected() {
        let mut content = CaptureWriter::default();
        let mut reasoning = CaptureWriter::default();
        let outcome = stream_response_to(
            stream(vec![chunk(serde_json::json!({
                "choices": [{"delta": {"reasoning": "think", "content": "answer"}}]
            }))]),
            true,
            &mut content,
            Some(&mut reasoning),
            None,
        )
        .await
        .expect("stream should render");

        assert_eq!(outcome.content, "answer");
        assert_eq!(String::from_utf8(content.bytes).unwrap(), "answer\n");
        assert_eq!(
            String::from_utf8(reasoning.bytes).unwrap(),
            "Reasoning:\nthink\n"
        );
    }

    #[tokio::test]
    async fn reports_cancellation_as_a_typed_interruption() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let mut content = CaptureWriter::default();
        let error = stream_response_to(
            Box::pin(futures_util::stream::pending()),
            false,
            &mut content,
            None::<&mut CaptureWriter>,
            Some(cancellation),
        )
        .await
        .expect_err("cancelled stream should fail");

        assert!(error.downcast_ref::<Interrupted>().is_some(), "{error}");
    }
}
