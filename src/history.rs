//! Execution history (`lait history`, see `docs/usage/ja/history.md`): every
//! successful chat/agent/workflow/prompt run is appended to a single,
//! user-wide JSONL log so a good prompt/response from earlier can be found
//! again without digging through shell history (which never keeps the
//! response side). Recording is opt-out via `--no-history`/
//! `default.history: false` — see `app::record_history`, the one place that
//! decides whether to call `record` at all.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
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
    /// — see `app::UsageTally::total`. `None` covers both "the server never
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
    if let Ok(dir) = std::env::var("XDG_DATA_HOME")
        && !dir.trim().is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context(
            "failed to determine the home directory (HOME/USERPROFILE is not set) to locate the \
             history file",
        )?;
    Ok(PathBuf::from(home).join(".local").join("share"))
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

fn load_all() -> Result<Vec<HistoryEntry>> {
    jsonl::load(&history_path()?)
}

/// Every recorded entry, most-recent first, numbered so `1` is the most
/// recent — the numbering `lait history show <n>`/`lait history search`
/// display and accept.
fn numbered_most_recent_first() -> Result<Vec<(usize, HistoryEntry)>> {
    let mut entries = load_all()?;
    entries.reverse();
    Ok(entries
        .into_iter()
        .enumerate()
        .map(|(i, entry)| (i + 1, entry))
        .collect())
}

/// The `limit` most recent entries, for `lait history`'s bare listing.
pub(crate) fn list(limit: usize) -> Result<Vec<(usize, HistoryEntry)>> {
    let mut entries = numbered_most_recent_first()?;
    entries.truncate(limit);
    Ok(entries)
}

/// The single entry numbered `index` (`1` = most recent). Fails clearly when
/// `index` is out of range.
pub(crate) fn show(index: usize) -> Result<HistoryEntry> {
    numbered_most_recent_first()?
        .into_iter()
        .find(|(number, _)| *number == index)
        .map(|(_, entry)| entry)
        .ok_or_else(|| anyhow!("no history entry numbered {index}"))
}

/// Every entry (most-recent first) whose prompt or response contains `query`
/// as a case-insensitive substring.
pub(crate) fn search(query: &str) -> Result<Vec<(usize, HistoryEntry)>> {
    let query = query.to_lowercase();
    Ok(numbered_most_recent_first()?
        .into_iter()
        .filter(|(_, entry)| {
            entry.prompt.to_lowercase().contains(&query)
                || entry.response.to_lowercase().contains(&query)
        })
        .collect())
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
fn summarize(text: &str) -> String {
    const MAX_CHARS: usize = 60;
    let flattened: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
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
    use super::{list, record, search, show, summarize};

    /// Runs `body` with `HOME`/`XDG_DATA_HOME` temporarily pointed at a
    /// fresh, empty directory, so the history file resolves under an
    /// isolated location instead of the real user's home. Serialized via a
    /// global lock: process environment variables are shared mutable state,
    /// and `cargo test`'s default threaded runner would otherwise let two
    /// history tests race on them.
    fn in_temp_home<T>(body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let dir = std::env::temp_dir().join(format!(
            "lait-test-history-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
