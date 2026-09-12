//! Execution history (`lait history`, see `docs/usage/ja/history.md`): every
//! successful chat/agent/workflow/prompt run is appended to a single,
//! user-wide JSONL log so a good prompt/response from earlier can be found
//! again without digging through shell history (which never keeps the
//! response side). Recording is opt-out via `--no-history`/
//! `default.history: false` — see `app::record_history`, the one place that
//! decides whether to call `record` at all.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::{
    cli::{HistoryAction, HistoryArgs},
    jsonl,
    response::Usage,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HistoryEntry {
    /// RFC 3339 UTC timestamp of when the run finished.
    pub(crate) timestamp: String,
    /// What kind of run this was: `"chat"`, `"agent"`, `"workflow"`, or
    /// `"prompt"`.
    pub(crate) kind: String,
    /// The model used, when the run has exactly one (chat/agent/prompt); a
    /// workflow can touch several models across its steps, so it records
    /// `None` here rather than picking one arbitrarily.
    pub(crate) model: Option<String>,
    pub(crate) prompt: String,
    pub(crate) response: String,
    /// The server-reported token usage accumulated over the run, when known
    /// — see `usage::UsageTally::total`. `None` covers both "the server never
    /// reports usage" and "a streamed chat turn that didn't request the
    /// usage chunk" (see `app::run_chat`), not just "usage is zero".
    pub(crate) usage: Option<Usage>,
}

/// Resolves `$XDG_DATA_HOME`, falling back to `$HOME/.local/share` (or
/// `%USERPROFILE%\.local\share` where `HOME` isn't set) per the XDG Base
/// Directory spec. A `dirs`-style crate is deliberately not used here: it
/// would map to platform-conventional directories (e.g. `~/Library/Application
/// Support` on macOS) rather than the literal `~/.local/share` this feature
/// is specified against.
fn xdg_data_home() -> Result<PathBuf> {
    crate::xdg::base_dir("XDG_DATA_HOME", &[".local", "share"], "the history file")
}

fn history_path() -> Result<PathBuf> {
    Ok(xdg_data_home()?.join("lait").join("history.jsonl"))
}

/// Appends one completed run to the history file, creating its parent
/// directory on first use. Only ever called after a run has actually
/// succeeded — see `app::record_history`.
pub(crate) fn record(
    kind: &str,
    model: Option<&str>,
    prompt: &str,
    response: &str,
    usage: Option<Usage>,
) -> Result<()> {
    let entry = HistoryEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        kind: kind.to_owned(),
        model: model.map(str::to_owned),
        prompt: prompt.to_owned(),
        response: response.to_owned(),
        usage,
    };
    jsonl::append(&history_path()?, [entry])
}

/// The `limit` most recent entries, for `lait history`'s bare listing.
/// Reads the log most-recent-first (see `jsonl::load_rev`) and stops as
/// soon as `limit` entries are collected, rather than deserializing (and
/// reversing) the whole file first just to keep its tail — a history log
/// only grows, so this makes `lait history`'s cost O(`limit`), not O(every
/// run ever recorded).
pub(crate) fn list(limit: usize) -> Result<Vec<(usize, HistoryEntry)>> {
    let mut entries = Vec::new();
    let mut number = 0_usize;
    jsonl::load_rev(&history_path()?, |entry: HistoryEntry| {
        number += 1;
        entries.push((number, entry));
        Ok(if entries.len() >= limit {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        })
    })?;
    Ok(entries)
}

/// The single entry numbered `index` (`1` = most recent). Fails clearly when
/// `index` is out of range. Stops reading the log as soon as `index` is
/// reached — see [`list`]'s doc comment for why this reads most-recent-first
/// instead of loading every entry.
pub(crate) fn show(index: usize) -> Result<HistoryEntry> {
    let mut found = None;
    let mut number = 0_usize;
    jsonl::load_rev(&history_path()?, |entry: HistoryEntry| {
        number += 1;
        Ok(if number == index {
            found = Some(entry);
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        })
    })?;
    found.ok_or_else(|| anyhow!("no history entry numbered {index}"))
}

/// Every entry (most-recent first) whose prompt or response contains `query`
/// as a case-insensitive substring. A match can be anywhere in the log, so
/// (unlike [`list`]/[`show`]) this still visits every entry — but streamed
/// one at a time via [`jsonl::load_rev`] rather than first collecting every
/// entry into a `Vec`, reversing it, and filtering a second pass.
pub(crate) fn search(query: &str) -> Result<Vec<(usize, HistoryEntry)>> {
    let query = query.to_lowercase();
    let mut matches = Vec::new();
    let mut number = 0_usize;
    jsonl::load_rev(&history_path()?, |entry: HistoryEntry| {
        number += 1;
        if contains_case_insensitive(&entry.prompt, &query)
            || contains_case_insensitive(&entry.response, &query)
        {
            matches.push((number, entry));
        }
        Ok(std::ops::ControlFlow::Continue(()))
    })?;
    Ok(matches)
}

/// Whether `haystack` contains `needle_lower` (already lowercased once by
/// the caller) as a case-insensitive substring, without allocating a
/// lowercased copy of `haystack` — unlike `haystack.to_lowercase().contains
/// (needle_lower)`, which `search` visits once per history entry, and a
/// response body can be tens of KB.
///
/// Takes the ASCII byte-comparison fast path whenever both sides are ASCII
/// (a `history search <text>` query and most model output usually are);
/// otherwise falls back to the always-correct `to_lowercase` + `contains`,
/// since ASCII case folding does not fold non-ASCII scripts (e.g. "É"/"é")
/// correctly.
fn contains_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let haystack = haystack.as_bytes();
        let needle = needle_lower.as_bytes();
        needle.len() <= haystack.len()
            && haystack
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle))
    } else {
        haystack.to_lowercase().contains(needle_lower)
    }
}

fn print_entry(number: usize, entry: &HistoryEntry) {
    let model = entry.model.as_deref().unwrap_or("-");
    println!(
        "{number}\t{}\t{}\t{model}\t{}",
        entry.timestamp,
        entry.kind,
        summarize(&entry.prompt)
    );
}

/// A one-line, ellipsized preview of a prompt/response for the list/search
/// table — the full text is only ever shown by `lait history show <n>`.
/// Builds the flattened (whitespace-collapsed) preview directly into one
/// `String`, stopping as soon as it has more than `MAX_CHARS` worth of
/// content, instead of collecting every word of `text` (a whole response,
/// which can be far longer than the handful of words this ever keeps) into
/// a `Vec<&str>` and joining all of it before truncating. Output is
/// identical to the join-everything-then-truncate approach it replaced —
/// the truncation below still takes exactly `MAX_CHARS` characters of the
/// (possibly word-splitting) flattened prefix — only the amount of `text`
/// actually visited is smaller.
fn summarize(text: &str) -> String {
    const MAX_CHARS: usize = 60;
    let mut flattened = String::new();
    for word in text.split_whitespace() {
        if !flattened.is_empty() {
            flattened.push(' ');
        }
        flattened.push_str(word);
        if flattened.chars().count() > MAX_CHARS {
            break;
        }
    }
    if flattened.chars().count() <= MAX_CHARS {
        flattened
    } else {
        let truncated: String = flattened.chars().take(MAX_CHARS).collect();
        format!("{truncated}...")
    }
}

/// Runs `lait history [--limit N] | show <N> | search <QUERY>` — a purely
/// local file operation (no async runtime needed, see
/// `app::needs_async_runtime`).
pub(crate) fn run(args: HistoryArgs) -> Result<()> {
    match args.action {
        None => {
            let entries = list(args.limit)?;
            if entries.is_empty() {
                println!("no history recorded yet");
                return Ok(());
            }
            for (number, entry) in &entries {
                print_entry(*number, entry);
            }
            Ok(())
        }
        Some(HistoryAction::Show(show_args)) => {
            if show_args.index == 0 {
                bail!("history index must be at least 1");
            }
            let entry = show(show_args.index)?;
            println!("timestamp: {}", entry.timestamp);
            println!("kind: {}", entry.kind);
            if let Some(model) = &entry.model {
                println!("model: {model}");
            }
            if let Some(usage) = entry.usage {
                println!("usage: {usage}");
            }
            println!("\nprompt:\n{}", entry.prompt);
            println!("\nresponse:\n{}", entry.response);
            Ok(())
        }
        Some(HistoryAction::Search(search_args)) => {
            let entries = search(&search_args.query)?;
            if entries.is_empty() {
                println!("no history entries match '{}'", search_args.query);
                return Ok(());
            }
            for (number, entry) in &entries {
                print_entry(*number, entry);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{contains_case_insensitive, list, record, search, show, summarize};

    /// Runs `body` with `HOME`/`XDG_DATA_HOME` temporarily pointed at a
    /// fresh, empty directory, so the history file resolves under an
    /// isolated location instead of the real user's home. Serialized via a
    /// global lock: process environment variables are shared mutable state,
    /// and `cargo test`'s default threaded runner would otherwise let two
    /// history tests race on them.
    fn in_temp_home<T>(body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let dir = crate::test_support::unique_temp_path("lait-test-history", "");
        std::fs::create_dir_all(&dir).unwrap();
        let original_home = std::env::var("HOME").ok();
        let original_xdg = std::env::var("XDG_DATA_HOME").ok();
        // SAFETY: serialized by `LOCK` above; no other thread reads these
        // while this closure runs.
        unsafe {
            std::env::set_var("HOME", &dir);
            std::env::remove_var("XDG_DATA_HOME");
        }
        let result = body();
        unsafe {
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match original_xdg {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn list_is_empty_when_nothing_has_been_recorded() {
        in_temp_home(|| {
            assert!(list(20).unwrap().is_empty());
        });
    }

    #[test]
    fn record_then_list_numbers_the_most_recent_entry_first() {
        in_temp_home(|| {
            record("chat", Some("m1"), "first", "first reply", None).unwrap();
            record("chat", Some("m1"), "second", "second reply", None).unwrap();

            let entries = list(20).unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].0, 1);
            assert_eq!(entries[0].1.prompt, "second");
            assert_eq!(entries[1].0, 2);
            assert_eq!(entries[1].1.prompt, "first");
        });
    }

    #[test]
    fn list_respects_the_limit() {
        in_temp_home(|| {
            for n in 0..5 {
                record("chat", None, &format!("p{n}"), "r", None).unwrap();
            }
            assert_eq!(list(2).unwrap().len(), 2);
        });
    }

    #[test]
    fn show_returns_the_entry_numbered_n() {
        in_temp_home(|| {
            record("chat", None, "first", "first reply", None).unwrap();
            record("chat", None, "second", "second reply", None).unwrap();

            assert_eq!(show(1).unwrap().prompt, "second");
            assert_eq!(show(2).unwrap().prompt, "first");
        });
    }

    #[test]
    fn show_fails_clearly_for_an_out_of_range_index() {
        in_temp_home(|| {
            record("chat", None, "only", "reply", None).unwrap();
            assert!(
                show(2)
                    .unwrap_err()
                    .to_string()
                    .contains("no history entry")
            );
        });
    }

    #[test]
    fn search_finds_a_case_insensitive_substring_in_prompt_or_response() {
        in_temp_home(|| {
            record("chat", None, "translate to French", "Bonjour", None).unwrap();
            record("chat", None, "summarize this", "a short summary", None).unwrap();

            let by_prompt = search("FRENCH").unwrap();
            assert_eq!(by_prompt.len(), 1);
            assert_eq!(by_prompt[0].1.prompt, "translate to French");

            let by_response = search("bonjour").unwrap();
            assert_eq!(by_response.len(), 1);
        });
    }

    #[test]
    fn search_returns_empty_for_no_match() {
        in_temp_home(|| {
            record("chat", None, "hello", "hi", None).unwrap();
            assert!(search("nope").unwrap().is_empty());
        });
    }

    /// `search` streams every entry through `contains_case_insensitive`
    /// rather than allocating a lowercased copy of each prompt/response —
    /// this pins its ASCII fast path and its non-ASCII fallback separately,
    /// since only the ASCII path skips that allocation.
    #[test]
    fn contains_case_insensitive_matches_via_the_ascii_fast_path() {
        assert!(contains_case_insensitive("translate to French", "french"));
        assert!(!contains_case_insensitive("translate to French", "german"));
        assert!(contains_case_insensitive("anything", ""));
        assert!(!contains_case_insensitive("hi", "hello"));
    }

    #[test]
    fn contains_case_insensitive_falls_back_correctly_for_non_ascii_text() {
        // "É" only case-folds to "é" via Unicode rules, not ASCII byte
        // comparison, so this only passes if the non-ASCII fallback runs.
        assert!(contains_case_insensitive("Café École", "école"));
        assert!(!contains_case_insensitive("Café École", "not present"));
    }

    #[test]
    fn summarize_leaves_short_text_unchanged() {
        assert_eq!(summarize("hello world"), "hello world");
    }

    #[test]
    fn summarize_ellipsizes_long_text_and_flattens_whitespace() {
        let long = "word ".repeat(30);
        let summary = summarize(&long);
        assert!(summary.ends_with("..."));
        assert!(!summary.contains('\n'));
    }
}
