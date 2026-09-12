//! Cassette files for `lait run --record`/`--replay` and `lait test` (see
//! docs/usage/ja/testing.md): each cassette records one LLM request/response
//! pair, keyed by the same content hash `cache::key` computes (base URL,
//! model, sampling, message history, tool definitions, response format —
//! never the API key), under a directory the caller names explicitly rather
//! than `cache.rs`'s fixed `.lait/cache/`.
//!
//! Unlike the response cache, where a miss just means "call the network", a
//! replay directory is meant to be the *only* source of truth for its run:
//! [`load`] fails loudly on a miss instead of silently falling through, so a
//! workflow change that starts sending a request nobody recorded is caught
//! immediately rather than quietly reaching the real network in what's
//! supposed to be a deterministic, offline test.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat,
};
use serde::{Deserialize, Serialize};

use crate::{async_io, response};

/// The request side of a cassette entry, kept only for human inspection —
/// matching a replay request to its cassette is entirely done by filename
/// (the content hash), so this is never read back by [`load`].
#[derive(Debug, Serialize)]
struct CassetteRequestRef<'a> {
    base_url: &'a str,
    model_id: &'a str,
    messages: &'a [ChatCompletionRequestMessage],
    tools: &'a [ChatCompletionTools],
    response_format: Option<&'a ResponseFormat>,
}

#[derive(Debug, Serialize)]
struct CassetteEntryRef<'a> {
    recorded_at: chrono::DateTime<chrono::Utc>,
    request: CassetteRequestRef<'a>,
    response: &'a response::ChatCompletionResponse,
}

/// The read side of a cassette entry: only `response` is ever used by
/// [`load`], but `serde` still needs a type to deserialize the whole file
/// into (`request`/`recorded_at` are simply dropped).
#[derive(Debug, Deserialize)]
struct CassetteEntry {
    response: response::ChatCompletionResponse,
}

fn entry_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

/// Saves one request/response pair to `dir` under `key` (see `cache::key`),
/// atomically (temp file in the same directory, then `rename` — see
/// `cache::save`/`checkpoint::save_cancellable`) on the bounded blocking I/O
/// worker (`async_io::run_blocking`). Creates `dir` (and any missing parent
/// directories) if it doesn't already exist.
///
/// Goes through `run_blocking` for the same reason `cache::save` does — see
/// its doc comment. Serializes synchronously first, borrowing every
/// argument, and only the resulting `body`/`path` (owned, so they can move
/// onto the worker thread) cross onto it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn save(
    dir: &Path,
    key: &str,
    base_url: &str,
    model_id: &str,
    messages: &[ChatCompletionRequestMessage],
    tools: &[ChatCompletionTools],
    response_format: Option<&ResponseFormat>,
    response: &response::ChatCompletionResponse,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<()> {
    let entry = CassetteEntryRef {
        recorded_at: chrono::Utc::now(),
        request: CassetteRequestRef {
            base_url,
            model_id,
            messages,
            tools,
            response_format,
        },
        response,
    };
    let body =
        serde_json::to_string_pretty(&entry).context("failed to serialize cassette entry")?;
    let path = entry_path(dir, key);
    async_io::run_blocking(
        move |_| {
            crate::storage::write_atomic(&path, body.as_bytes())
                .with_context(|| format!("failed to save cassette entry to '{}'", path.display()))
        },
        cancellation,
    )
    .await
}

/// Reads back the cassette entry for `key` in `dir`. Unlike `cache::load`, a
/// missing entry is a hard error (`--replay`'s whole point is to never touch
/// the network): the message names the directory, the file it looked for,
/// and `model_id`, so a mismatch is easy to diagnose (a workflow/input/vars
/// change since the recording, or a cassette directory that was never
/// populated for this request at all).
///
/// Goes through `async_io::read_to_string_cancellable` for the same reason
/// `cache::load` does — see its doc comment.
pub(crate) async fn load(
    dir: &Path,
    key: &str,
    model_id: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<response::ChatCompletionResponse> {
    let path = entry_path(dir, key);
    let body =
        match async_io::read_to_string_cancellable(&path, cancellation, async_io::MAX_READ_BYTES)
            .await
        {
            Ok(body) => body,
            Err(error) if async_io::is_not_found(&error) => {
                bail!(
                    "no recorded cassette for this request (model '{model_id}') at '{}'; run `lait \
                 run --record {}` first against the same workflow/input/vars, or check that \
                 they still match this recording",
                    path.display(),
                    dir.display(),
                );
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read cassette '{}'", path.display()));
            }
        };
    let entry: CassetteEntry = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse cassette entry '{}'", path.display()))?;
    Ok(entry.response)
}

#[cfg(test)]
mod tests {
    use super::{load, save};
    use crate::response::ChatCompletionResponse;

    fn sample_response(content: &str) -> ChatCompletionResponse {
        serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "model-a",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }],
        }))
        .expect("sample response should deserialize")
    }

    #[tokio::test]
    async fn saves_and_loads_a_cassette_entry() {
        let dir = tempfile_dir();
        let response = sample_response("hello");
        save(
            dir.path(),
            "key-1",
            "http://x",
            "model-a",
            &[],
            &[],
            None,
            &response,
            None,
        )
        .await
        .expect("save should succeed");

        let loaded = load(dir.path(), "key-1", "model-a", None)
            .await
            .expect("load should succeed");
        assert_eq!(crate::response::content_text(&loaded), "hello");
    }

    /// `load` now reads through `async_io::read_to_string_cancellable`
    /// rather than a bare `std::fs::read_to_string` — pins that the
    /// crate-wide 16MiB read limit applies here too, as its own distinct
    /// error rather than the missing-entry message. Mirrors
    /// `async_io::read_to_string_sync_rejects_a_file_beyond_max_read_bytes`.
    #[tokio::test]
    async fn load_rejects_a_cassette_entry_beyond_max_read_bytes() {
        let dir = tempfile_dir();
        std::fs::write(
            dir.path().join("big-key.json"),
            vec![b'a'; crate::async_io::MAX_READ_BYTES + 1],
        )
        .unwrap();

        let error = load(dir.path(), "big-key", "model-a", None)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("read limit"),
            "error: {error:#}"
        );
    }

    #[tokio::test]
    async fn load_fails_clearly_when_the_key_has_no_cassette() {
        let dir = tempfile_dir();
        let error = load(dir.path(), "missing-key", "model-a", None)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("model-a"), "{message}");
        assert!(message.contains("--record"), "{message}");
    }

    #[tokio::test]
    async fn load_reports_a_parse_failure_distinctly_from_a_missing_entry() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("bad-key.json"), "not json").unwrap();
        let error = load(dir.path(), "bad-key", "model-a", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("bad-key.json"), "{error}");
    }

    #[tokio::test]
    async fn save_creates_missing_directories() {
        let dir = tempfile_dir();
        let nested = dir.path().join("nested").join("cassettes");
        let response = sample_response("hi");
        save(
            &nested,
            "k",
            "http://x",
            "model-a",
            &[],
            &[],
            None,
            &response,
            None,
        )
        .await
        .expect("save should create missing directories");
        assert!(nested.join("k.json").is_file());
    }

    /// A minimal `tempfile`-free temporary directory helper for this
    /// module's unit tests (which, unlike `tests/*.rs` integration tests,
    /// cannot reach `tests/support`'s fixture helpers) — a process/time
    /// unique path under `std::env::temp_dir()`, removed when the guard
    /// drops.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempfile_dir() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "lait-cassette-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("failed to create temp dir for test");
        TempDir(path)
    }
}
