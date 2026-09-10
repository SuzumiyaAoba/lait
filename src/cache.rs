//! The response disk cache (`--cache`/`default.cache`, `lait cache clear`):
//! a completion response saved to `.lait/cache/<key>.json` after a
//! successful request, keyed by everything that determines what the server
//! would return (base URL, model, sampling, message history, tool
//! definitions, response format) but deliberately *not* the API key — two
//! requests that only differ in credentials should share a cache entry, and
//! a key must never itself be a secret sitting in a cache file. Checked by
//! `engine::RequestSettings::complete_recorded`, the single choke point
//! every non-streamed completion request already goes through (see
//! `docs/usage/ja/config.md`'s キャッシュ section) — a tool loop's later
//! rounds therefore each get their own cache entry, keyed by their own
//! (longer) message history, rather than the whole loop being cached as one
//! unit. Streamed (`--stream`) responses never go through
//! `complete_recorded` and are never cached.
//!
//! Like `checkpoint.rs`'s `.lait/runs/`, this is a project-local concept
//! (relative to the current directory, not XDG), and a write is a whole-file
//! temp-then-`rename` for the same crash-safety reason — see `checkpoint::save_cancellable`'s
//! doc comment for why `jsonl.rs`'s append-only primitives don't fit here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    async_io,
    cli::{CacheAction, CacheCommand},
    engine::SamplingOverrides,
    response,
};

/// The directory every cache entry lives under, relative to the current
/// directory — see `checkpoint::RUNS_DIR`, which this mirrors.
const CACHE_DIR: &str = ".lait/cache";

/// The `key` payload's shape, borrowing every field: this is `Serialize`d
/// straight into the hasher (see `key`) instead of first being collected
/// into an owned `serde_json::Value` tree (as a `json!` literal would do)
/// and then re-serialized — the same borrowed-struct pattern
/// `template::RenderScope` uses for the same reason. Field order here is
/// exactly the field declaration order, since `derive(Serialize)` on a
/// struct serializes as a map in declaration order — matching the `json!`
/// literal this replaced (`preserve_order` is irrelevant either way, since
/// neither form goes through a `HashMap`).
#[derive(Serialize)]
struct CacheKeyInput<'a> {
    base_url: &'a str,
    model_id: &'a str,
    // `ReasoningEffort` derives `Serialize` (see `reasoning.rs`'s module
    // doc for why it moved out of `cli.rs`), and its `#[serde(rename)]`
    // attributes are pinned to match `as_str()` exactly
    // (`reasoning::tests::serialized_name_matches_as_str`) — so this
    // field's byte encoding is unchanged from when it held `as_str()`'s
    // `Option<&'static str>` directly (see
    // `key_output_is_pinned_against_a_fixed_encoding` below).
    reasoning_effort: Option<crate::reasoning::ReasoningEffort>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<u32>,
    messages: &'a [ChatCompletionRequestMessage],
    tools: &'a [ChatCompletionTools],
    response_format: Option<&'a ResponseFormat>,
}

/// Computes the cache key for a request: a SHA-256 hex digest over a
/// canonical JSON encoding of every input that determines the response.
/// `sha2` rather than `std::collections::hash_map::DefaultHasher` because
/// the latter's algorithm is explicitly not guaranteed stable across Rust
/// releases, and a cache key needs to keep matching across `cargo` upgrades
/// for entries already on disk to stay useful. `CacheKeyInput`'s fields are
/// declared in a fixed order (not a `HashMap`), so the encoding is
/// deterministic without needing `serde_json`'s `preserve_order` feature to
/// do any extra work here. Deliberately excludes `api_key`.
///
/// Serializes straight into the hasher via `serde_json::to_writer` (`Sha256`
/// implements `io::Write` through the `digest` crate's `std`-feature
/// `CoreWrapper` impl) rather than building an intermediate `Vec<u8>` first
/// — one pass over `messages`/`tools` (which, for a tool loop's later
/// rounds, can be the largest input here) instead of two.
pub(crate) fn key(
    base_url: &str,
    model_id: &str,
    sampling: SamplingOverrides,
    messages: &[ChatCompletionRequestMessage],
    tools: &[ChatCompletionTools],
    response_format: Option<&ResponseFormat>,
) -> Result<String> {
    let payload = CacheKeyInput {
        base_url,
        model_id,
        reasoning_effort: sampling.reasoning_effort,
        temperature: sampling.temperature,
        top_p: sampling.top_p,
        max_tokens: sampling.max_tokens,
        messages,
        tools,
        response_format,
    };
    let mut hasher = Sha256::new();
    serde_json::to_writer(&mut hasher, &payload)
        .context("failed to serialize the cache key input")?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Debug, Deserialize)]
struct CacheEntry {
    created_at: chrono::DateTime<chrono::Utc>,
    response: response::ChatCompletionResponse,
}

/// The borrowed shape of [`CacheEntry`] used to serialize a save without
/// cloning the response just to own it alongside `created_at`.
#[derive(Debug, Serialize)]
struct CacheEntryRef<'a> {
    created_at: chrono::DateTime<chrono::Utc>,
    response: &'a response::ChatCompletionResponse,
}

fn entry_path(key: &str) -> PathBuf {
    Path::new(CACHE_DIR).join(format!("{key}.json"))
}

/// Reads back the cache entry for `key`, when one exists and (if `ttl_secs`
/// is set) is not older than that many seconds. A missing file, a parse
/// failure (e.g. a cache format from a future lait version), or an expired
/// entry are all treated as a plain miss (`Ok(None)`) rather than an error —
/// a cache is an optimization, and refusing to serve a request over a stale
/// or unreadable cache entry would defeat the point.
///
/// Goes through `async_io::read_to_string_cancellable` rather than a plain
/// synchronous read: unlike `report::emit_output`'s one-shot final write
/// (see its own doc comment for why that one stays synchronous), a cache
/// lookup runs on every `complete_recorded` call, including from concurrent
/// workflow branches (`for_each`/`parallel`), so it needs to observe the
/// same cancellation a request's own timeout would.
///
/// `now` is a parameter (production call sites pass `chrono::Utc::now()`)
/// rather than read internally, so TTL expiry can be tested deterministically
/// instead of depending on real elapsed time — see
/// `expired_entries_are_treated_as_a_miss`.
pub(crate) async fn load(
    key: &str,
    ttl_secs: Option<u64>,
    now: chrono::DateTime<chrono::Utc>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Option<response::ChatCompletionResponse>> {
    let path = entry_path(key);
    let body =
        match async_io::read_to_string_cancellable(&path, cancellation, async_io::MAX_READ_BYTES)
            .await
        {
            Ok(body) => body,
            Err(error) if async_io::is_not_found(&error) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read '{}'", path.display()));
            }
        };
    let Ok(entry) = serde_json::from_str::<CacheEntry>(&body) else {
        return Ok(None);
    };
    if let Some(ttl_secs) = ttl_secs {
        let age = now.signed_duration_since(entry.created_at);
        if age < chrono::Duration::zero() || age.num_seconds() as u64 > ttl_secs {
            return Ok(None);
        }
    }
    Ok(Some(entry.response))
}

/// Writes `response` to `key`'s cache entry, atomically (temp file in the
/// same directory, then `rename` — see `storage::write_atomic`). `now`
/// becomes the entry's `created_at` — see `load`'s doc comment for why it's
/// a parameter rather than read internally.
pub(crate) fn save(
    key: &str,
    response: &response::ChatCompletionResponse,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let path = entry_path(key);
    let entry = CacheEntryRef {
        created_at: now,
        response,
    };
    let body = serde_json::to_string_pretty(&entry).context("failed to serialize cache entry")?;
    crate::storage::write_atomic(&path, body.as_bytes())
        .with_context(|| format!("failed to save cache entry to '{}'", path.display()))?;
    Ok(())
}

/// Deletes every cached response under `CACHE_DIR`. A missing directory
/// (nothing was ever cached) is not an error.
fn clear() -> Result<()> {
    match std::fs::remove_dir_all(CACHE_DIR) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove directory '{CACHE_DIR}'"))
        }
    }
}

/// Runs `lait cache clear`.
pub(crate) fn run(command: CacheCommand) -> Result<()> {
    match command.action {
        CacheAction::Clear => {
            clear()?;
            println!("cache cleared");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{key, load, save};
    use crate::engine::SamplingOverrides;
    use crate::response;

    /// `load` now reads through `async_io::read_to_string_cancellable`
    /// rather than a bare `std::fs::read_to_string` — pins that the
    /// crate-wide 16MiB read limit applies here too, as an actual error
    /// rather than the "treat as a miss" fallback a parse failure gets.
    /// Mirrors `async_io::read_to_string_sync_rejects_a_file_beyond_max_read_bytes`.
    #[tokio::test]
    async fn load_rejects_a_cache_entry_beyond_max_read_bytes() {
        crate::test_support::in_temp_dir_async("lait-cache-read-limit", async {
            std::fs::create_dir_all(super::CACHE_DIR).unwrap();
            let path = std::path::Path::new(super::CACHE_DIR).join("big-key.json");
            std::fs::write(&path, vec![b'a'; crate::async_io::MAX_READ_BYTES + 1]).unwrap();

            let error = load("big-key", None, chrono::Utc::now(), None)
                .await
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("read limit"),
                "error: {error:#}"
            );
        })
        .await;
    }

    fn sample_response() -> response::ChatCompletionResponse {
        serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "model-a",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop",
            }],
        }))
        .expect("sample response should deserialize")
    }

    /// `load`/`save` take `now` as a parameter rather than reading
    /// `chrono::Utc::now()` internally (B7), specifically so TTL expiry can
    /// be tested deterministically instead of needing a real sleep — this is
    /// that test.
    #[tokio::test]
    async fn expired_entries_are_treated_as_a_miss() {
        crate::test_support::in_temp_dir_async("lait-cache-ttl", async {
            let saved_at = chrono::Utc::now();
            let response = sample_response();
            save("ttl-key", &response, saved_at).expect("save should succeed");

            // Just inside the TTL: still a hit.
            let just_before_expiry = saved_at + chrono::Duration::seconds(59);
            assert!(
                load("ttl-key", Some(60), just_before_expiry, None)
                    .await
                    .expect("load should succeed")
                    .is_some(),
                "an entry younger than its TTL should still be a hit"
            );

            // Past the TTL: a miss, not an error.
            let after_expiry = saved_at + chrono::Duration::seconds(61);
            assert!(
                load("ttl-key", Some(60), after_expiry, None)
                    .await
                    .expect("load should succeed")
                    .is_none(),
                "an entry older than its TTL should be treated as a miss"
            );
        })
        .await;
    }

    /// Pins `key`'s output against two fixed inputs, hardcoded as of the
    /// `json!`-plus-`to_vec` implementation. This is the only test in this
    /// module that would catch a change to the key's byte encoding (the
    /// other tests below only check "same input -> same key" and "different
    /// input -> different key", which a differently-encoded-but-still-
    /// deterministic implementation would still satisfy) — a changed key
    /// silently invalidates every existing `.lait/cache/*.json` entry and
    /// every `--replay` cassette (see `cassette::load`, keyed the same way),
    /// so changing the encoding must be a deliberate, reviewed decision, not
    /// an accidental side effect of an unrelated refactor.
    #[test]
    fn key_output_is_pinned_against_a_fixed_encoding() {
        let empty = key(
            "http://x",
            "m",
            SamplingOverrides::default(),
            &[],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(
            empty,
            "4f31bb16957772afd4b0e91ec7cc92478f9b67169bbcb8b9271768cf9e6e92ef"
        );

        let sampling = SamplingOverrides {
            reasoning_effort: Some(crate::reasoning::ReasoningEffort::High),
            temperature: Some(0.5),
            top_p: Some(0.9),
            max_tokens: Some(256),
        };
        let messages = vec![crate::llm::user_message("hello", &[]).unwrap()];
        let tools = vec![async_openai::types::chat::ChatCompletionTools::Function(
            async_openai::types::chat::ChatCompletionTool {
                function: async_openai::types::chat::FunctionObject {
                    name: "tool__example".to_owned(),
                    description: Some("an example tool".to_owned()),
                    parameters: Some(serde_json::json!({"type": "object"})),
                    strict: None,
                },
            },
        )];
        let response_format =
            crate::schema::build_json_schema(serde_json::json!({"type": "object"}), "out").unwrap();
        let rich = key(
            "http://x",
            "m",
            sampling,
            &messages,
            &tools,
            Some(&response_format),
        )
        .unwrap();
        assert_eq!(
            rich,
            "c709b731bbd10d404af85bfb35cc73063ad9787b0ad40ae994a08fabce04f47f"
        );
    }

    #[test]
    fn the_same_inputs_produce_the_same_key() {
        let a = key(
            "http://x",
            "m",
            SamplingOverrides::default(),
            &[],
            &[],
            None,
        )
        .unwrap();
        let b = key(
            "http://x",
            "m",
            SamplingOverrides::default(),
            &[],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_different_base_url_produces_a_different_key() {
        let a = key(
            "http://x",
            "m",
            SamplingOverrides::default(),
            &[],
            &[],
            None,
        )
        .unwrap();
        let b = key(
            "http://y",
            "m",
            SamplingOverrides::default(),
            &[],
            &[],
            None,
        )
        .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_different_model_id_produces_a_different_key() {
        let a = key(
            "http://x",
            "m1",
            SamplingOverrides::default(),
            &[],
            &[],
            None,
        )
        .unwrap();
        let b = key(
            "http://x",
            "m2",
            SamplingOverrides::default(),
            &[],
            &[],
            None,
        )
        .unwrap();
        assert_ne!(a, b);
    }
}
