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
//! temp-then-`rename` for the same crash-safety reason — see `checkpoint::save`'s
//! doc comment for why `jsonl.rs`'s append-only primitives don't fit here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTools, ResponseFormat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    cli::{CacheAction, CacheCommand},
    engine::SamplingOverrides,
    response,
};

/// The directory every cache entry lives under, relative to the current
/// directory — see `checkpoint::RUNS_DIR`, which this mirrors.
const CACHE_DIR: &str = ".lait/cache";

/// Computes the cache key for a request: a SHA-256 hex digest over a
/// canonical JSON encoding of every input that determines the response.
/// `sha2` rather than `std::collections::hash_map::DefaultHasher` because
/// the latter's algorithm is explicitly not guaranteed stable across Rust
/// releases, and a cache key needs to keep matching across `cargo` upgrades
/// for entries already on disk to stay useful. Field order in the `json!`
/// call below is fixed by the macro (not a `HashMap`), so the encoding is
/// deterministic without needing `serde_json`'s `preserve_order` feature to
/// do any extra work here. Deliberately excludes `api_key`.
pub(crate) fn key(
    base_url: &str,
    model_id: &str,
    sampling: SamplingOverrides,
    messages: &[ChatCompletionRequestMessage],
    tools: &[ChatCompletionTools],
    response_format: Option<&ResponseFormat>,
) -> Result<String> {
    // `ReasoningEffort` implements neither `Serialize` (a CLI/config-only
    // type) nor `Display`, so its own `as_str()` (already the canonical
    // lowercase form used across the CLI/YAML) stands in for it here.
    let payload = serde_json::json!({
        "base_url": base_url,
        "model_id": model_id,
        "reasoning_effort": sampling.reasoning_effort.map(crate::cli::ReasoningEffort::as_str),
        "temperature": sampling.temperature,
        "top_p": sampling.top_p,
        "max_tokens": sampling.max_tokens,
        "messages": messages,
        "tools": tools,
        "response_format": response_format,
    });
    let serialized =
        serde_json::to_vec(&payload).context("failed to serialize the cache key input")?;
    let mut hasher = Sha256::new();
    hasher.update(&serialized);
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
pub(crate) fn load(
    key: &str,
    ttl_secs: Option<u64>,
) -> Result<Option<response::ChatCompletionResponse>> {
    let path = entry_path(key);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read '{}'", path.display()));
        }
    };
    let Ok(entry) = serde_json::from_str::<CacheEntry>(&body) else {
        return Ok(None);
    };
    if let Some(ttl_secs) = ttl_secs {
        let age = chrono::Utc::now().signed_duration_since(entry.created_at);
        if age < chrono::Duration::zero() || age.num_seconds() as u64 > ttl_secs {
            return Ok(None);
        }
    }
    Ok(Some(entry.response))
}

/// Writes `response` to `key`'s cache entry, atomically (temp file in the
/// same directory, then `rename` — see `checkpoint::save`).
pub(crate) fn save(key: &str, response: &response::ChatCompletionResponse) -> Result<()> {
    let path = entry_path(key);
    let dir = path
        .parent()
        .expect("entry_path always returns a path under CACHE_DIR");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create directory '{}'", dir.display()))?;
    let entry = CacheEntryRef {
        created_at: chrono::Utc::now(),
        response,
    };
    let body = serde_json::to_string_pretty(&entry).context("failed to serialize cache entry")?;
    let tmp_path = Path::new(CACHE_DIR).join(format!("{key}.json.tmp"));
    std::fs::write(&tmp_path, body)
        .with_context(|| format!("failed to write '{}'", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path)
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
    use super::key;
    use crate::engine::SamplingOverrides;

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
