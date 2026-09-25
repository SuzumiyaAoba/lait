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
    cli::{HistoryAction, HistoryArgs, HistoryFilterArgs},
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

/// Which entries `lait history`'s listing and `search` keep
/// (`--kind`/`--model`/`--since`). Entry numbers are still counted over
/// *every* entry, so a filtered listing's numbers stay valid for `show`.
#[derive(Debug, Default)]
pub(crate) struct Filter {
    kind: Option<String>,
    model: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
}

impl Filter {
    /// Builds a filter from the CLI's raw values, parsing `--since` against
    /// `now` (see [`parse_since`]).
    pub(crate) fn new(
        kind: Option<String>,
        model: Option<String>,
        since: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Self> {
        Ok(Self {
            kind,
            model,
            since: since.map(|since| parse_since(since, now)).transpose()?,
        })
    }

    fn from_args(args: &HistoryFilterArgs, now: chrono::DateTime<chrono::Utc>) -> Result<Self> {
        Self::new(
            args.kind.clone(),
            args.model.clone(),
            args.since.as_deref(),
            now,
        )
    }

    fn matches(&self, entry: &HistoryEntry) -> bool {
        if self.kind.as_deref().is_some_and(|kind| entry.kind != kind) {
            return false;
        }
        if let Some(model) = &self.model
            && !entry
                .model
                .as_deref()
                .is_some_and(|entry_model| entry_model.contains(model.as_str()))
        {
            return false;
        }
        if let Some(since) = self.since {
            // An entry whose timestamp doesn't parse can't be shown to be
            // recent enough, so a `--since` filter leaves it out.
            return chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
                .is_ok_and(|timestamp| timestamp >= since);
        }
        true
    }
}

/// Parses `--since`: `YYYY-MM-DD` (UTC midnight), an RFC 3339 timestamp, or
/// `<N><unit>` with unit `m`/`h`/`d`/`w`, counted back from `now`.
fn parse_since(
    since: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<chrono::DateTime<chrono::Utc>> {
    let since = since.trim();
    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(since) {
        return Ok(timestamp.with_timezone(&chrono::Utc));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(since, "%Y-%m-%d") {
        return Ok(date.and_time(chrono::NaiveTime::MIN).and_utc());
    }
    let unit_start = since
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(since.len());
    let (amount, unit) = since.split_at(unit_start);
    let amount: i64 = amount.parse().map_err(|_| since_error(since))?;
    let duration = match unit {
        "m" => chrono::Duration::try_minutes(amount),
        "h" => chrono::Duration::try_hours(amount),
        "d" => chrono::Duration::try_days(amount),
        "w" => chrono::Duration::try_weeks(amount),
        _ => None,
    }
    .ok_or_else(|| since_error(since))?;
    now.checked_sub_signed(duration)
        .ok_or_else(|| since_error(since))
}

fn since_error(since: &str) -> anyhow::Error {
    anyhow!(
        "invalid --since '{since}'; expected a date (2026-09-01), an RFC 3339 timestamp, or a \
         duration like 30m/12h/7d/2w"
    )
}

/// The `limit` most recent entries, for `lait history`'s bare listing.
/// Reads the log most-recent-first (see `jsonl::load_rev`) and stops as
/// soon as `limit` entries are collected, rather than deserializing (and
/// reversing) the whole file first just to keep its tail — a history log
/// only grows, so this makes `lait history`'s cost O(`limit`), not O(every
/// run ever recorded).
pub(crate) fn list(limit: usize, filter: &Filter) -> Result<Vec<(usize, HistoryEntry)>> {
    let mut entries = Vec::new();
    let mut number = 0_usize;
    jsonl::load_rev(&history_path()?, |entry: HistoryEntry| {
        number += 1;
        if !filter.matches(&entry) {
            return Ok(std::ops::ControlFlow::Continue(()));
        }
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
pub(crate) fn search(query: &str, filter: &Filter) -> Result<Vec<(usize, HistoryEntry)>> {
    let query = query.to_lowercase();
    let mut matches = Vec::new();
    let mut number = 0_usize;
    jsonl::load_rev(&history_path()?, |entry: HistoryEntry| {
        number += 1;
        if filter.matches(&entry)
            && (contains_case_insensitive(&entry.prompt, &query)
                || contains_case_insensitive(&entry.response, &query))
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

/// One listed entry as `--json` prints it: the entry's own fields plus the
/// `number` `show` accepts.
#[derive(Serialize)]
struct NumberedEntry<'a> {
    number: usize,
    #[serde(flatten)]
    entry: &'a HistoryEntry,
}

fn print_entries(entries: &[(usize, HistoryEntry)], json: bool, empty_message: &str) -> Result<()> {
    if json {
        let numbered: Vec<NumberedEntry<'_>> = entries
            .iter()
            .map(|(number, entry)| NumberedEntry {
                number: *number,
                entry,
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&numbered)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("{empty_message}");
        return Ok(());
    }
    for (number, entry) in entries {
        print_entry(*number, entry);
    }
    Ok(())
}

/// Runs `lait history [--limit N] | show <N> | search <QUERY>` — a purely
/// local file operation (no async runtime needed, see
/// `app::needs_async_runtime`). `--kind`/`--model`/`--since` narrow the
/// listing and `search`; `show` always addresses an entry by number.
pub(crate) fn run(args: HistoryArgs) -> Result<()> {
    let now = chrono::Utc::now();
    match args.action {
        None => {
            let filter = Filter::from_args(&args.filter, now)?;
            print_entries(
                &list(args.limit, &filter)?,
                args.filter.json,
                "no history recorded yet",
            )
        }
        Some(HistoryAction::Show(show_args)) => {
            if show_args.index == 0 {
                bail!("history index must be at least 1");
            }
            let entry = show(show_args.index)?;
            if show_args.json || args.filter.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&NumberedEntry {
                        number: show_args.index,
                        entry: &entry,
                    })?
                );
                return Ok(());
            }
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
            // Options may sit on either side of `search`; the subcommand's
            // own take precedence.
            let merged = HistoryFilterArgs {
                kind: search_args.filter.kind.or(args.filter.kind),
                model: search_args.filter.model.or(args.filter.model),
                since: search_args.filter.since.or(args.filter.since),
                json: search_args.filter.json || args.filter.json,
            };
            let filter = Filter::from_args(&merged, now)?;
            print_entries(
                &search(&search_args.query, &filter)?,
                merged.json,
                &format!("no history entries match '{}'", search_args.query),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Filter, contains_case_insensitive, list, parse_since, record, search, show, summarize,
    };

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
            assert!(list(20, &Filter::default()).unwrap().is_empty());
        });
    }

    #[test]
    fn record_then_list_numbers_the_most_recent_entry_first() {
        in_temp_home(|| {
            record("chat", Some("m1"), "first", "first reply", None).unwrap();
            record("chat", Some("m1"), "second", "second reply", None).unwrap();

            let entries = list(20, &Filter::default()).unwrap();
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
            assert_eq!(list(2, &Filter::default()).unwrap().len(), 2);
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

            let by_prompt = search("FRENCH", &Filter::default()).unwrap();
            assert_eq!(by_prompt.len(), 1);
            assert_eq!(by_prompt[0].1.prompt, "translate to French");

            let by_response = search("bonjour", &Filter::default()).unwrap();
            assert_eq!(by_response.len(), 1);
        });
    }

    #[test]
    fn search_returns_empty_for_no_match() {
        in_temp_home(|| {
            record("chat", None, "hello", "hi", None).unwrap();
            assert!(search("nope", &Filter::default()).unwrap().is_empty());
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

    #[test]
    fn parse_since_accepts_dates_timestamps_and_durations() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(
            parse_since("2026-09-01", now).unwrap().to_rfc3339(),
            "2026-09-01T00:00:00+00:00"
        );
        assert_eq!(
            parse_since("2026-09-24T10:00:00+09:00", now)
                .unwrap()
                .to_rfc3339(),
            "2026-09-24T01:00:00+00:00"
        );
        assert_eq!(
            parse_since("12h", now).unwrap().to_rfc3339(),
            "2026-09-25T00:00:00+00:00"
        );
        assert_eq!(
            parse_since("1w", now).unwrap().to_rfc3339(),
            "2026-09-18T12:00:00+00:00"
        );
        assert!(parse_since("7x", now).is_err());
        assert!(parse_since("yesterday", now).is_err());
    }

    #[test]
    fn filters_by_kind_model_and_since_while_keeping_global_numbers() {
        in_temp_home(|| {
            record("chat", Some("gpt-local"), "one", "r1", None).unwrap();
            record("workflow", None, "two", "r2", None).unwrap();
            record("chat", Some("other"), "three", "r3", None).unwrap();

            let chats =
                Filter::new(Some("chat".to_owned()), None, None, chrono::Utc::now()).unwrap();
            let numbers: Vec<usize> = list(20, &chats)
                .unwrap()
                .into_iter()
                .map(|(number, _)| number)
                .collect();
            assert_eq!(numbers, vec![1, 3]);

            let local =
                Filter::new(None, Some("local".to_owned()), None, chrono::Utc::now()).unwrap();
            let found = search("r", &local).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].0, 3);
            assert_eq!(found[0].1.prompt, "one");

            let future = Filter::new(None, None, Some("2999-01-01"), chrono::Utc::now()).unwrap();
            assert!(list(20, &future).unwrap().is_empty());
        });
    }
}
