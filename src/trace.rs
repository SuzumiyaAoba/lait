//! Run-trace collection: a flat, timestamped JSONL log of every model
//! completion request and tool invocation one `lait run --trace-file`
//! invocation makes, attributed by the same `label` `usage::UsageTally`
//! already keys its per-label token tallies with (a workflow step id,
//! `"chat"`, `agent '<name>'`, `subagent '<name>'`, ...) — see
//! `engine::RequestSettings::usage_label`'s doc comment for where that label
//! comes from. [`TraceCollector`] lives in `engine::RunContext` exactly the
//! way `usage::UsageTally` does (see that module's doc comment): both are
//! internal-mutability accumulators recorded into via `&self` from
//! concurrently running tasks (`parallel`/concurrent `for_each` branches),
//! and both are read back once, after a run finishes, to produce a summary/
//! file write.
//!
//! Attribute keys on [`TraceEvent::attributes`] follow the OpenTelemetry
//! GenAI semantic conventions (`gen_ai.*`, still under active development at
//! the time this was written) so `lait trace export` (see [`otlp`]) can
//! forward them to an OTLP collector unchanged. A `lait.*`-prefixed key (`lait.source`, `lait.tool.round`,
//! ...) is this crate's own bookkeeping with no GenAI semconv equivalent.
//!
//! This is deliberately a flat per-event log, not a span tree with parent/
//! child ids: threading a "current parent span" through every nested async
//! call (`RequestSettings::complete`, `ToolLoop::append_tool_calls`,
//! `call_subagent_tool`, `workflow::exec::run_steps`) would touch signatures
//! across half the engine for a benefit `label`-based grouping already gives
//! for free — a subagent's own model calls already record under their own
//! `subagent '<name>'` label, an MCP/shell/subagent tool call already
//! carries its own qualified tool name in `gen_ai.tool.name`. See
//! docs/usage/ja/trace.md for how a reader reconstructs a run's shape from
//! `label`/`seq` alone.

use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::storage;

/// `lait trace export`: a written trace file as OTLP/HTTP JSON spans. Split
/// out of this file since it is a separate consumer of [`TraceEvent`]s,
/// with its own wire format, rather than part of recording them.
mod otlp;

pub(crate) use otlp::export_otlp;

/// One recorded event: a model completion request (`operation ==
/// "chat"`, OTel's `gen_ai.operation.name` vocabulary) or a tool call
/// attempt (`operation == "execute_tool"`), including one denied by
/// `tool_policy`/`--approve-tools` — see this module's doc comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TraceEvent {
    /// Monotonically increasing per-run sequence number, assigned when the
    /// event is recorded. `TraceCollector::events` sorts by this so
    /// concurrent branches recording out of timestamp order (system clock
    /// resolution, or a task simply scheduled slightly later than one that
    /// started sooner) still produce a deterministic reading order.
    pub(crate) seq: u64,
    /// `&'static str` at every call site (only ever `"chat"`/`"execute_tool"`
    /// literals), stored owned so `TraceEvent` can round-trip through
    /// `serde_json::from_str` when `lait trace show` reads a file back — a
    /// `Deserialize`d `&'de str` borrowed from the input line cannot satisfy
    /// a `&'static str` field.
    pub(crate) operation: String,
    /// The same label `usage::UsageTally` records this event's usage under.
    pub(crate) label: String,
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: DateTime<Utc>,
    pub(crate) duration_ms: i64,
    /// OTel GenAI attributes (`gen_ai.request.model`,
    /// `gen_ai.usage.input_tokens`, `gen_ai.tool.name`, ...), plus this
    /// crate's own `lait.*`-prefixed bookkeeping. A `serde_json::Map`
    /// rather than a fixed struct so a new attribute doesn't need its own
    /// field and `#[serde(skip_serializing_if)]` — every event kind only
    /// ever sets a handful of the attributes any other kind might.
    pub(crate) attributes: Map<String, Value>,
}

/// Accumulates every [`TraceEvent`] over one `lait` run, mirroring
/// `usage::UsageTally`'s shape (see this module's doc comment) so recording
/// tolerates the same concurrent callers. Always present on `RunContext` and
/// always recording — like `UsageTally`, whether anything is ever done with
/// the result (written to `--trace-file`) is a separate, later decision, not
/// something recording itself needs to know about.
#[derive(Default)]
pub(crate) struct TraceCollector {
    events: Mutex<Vec<TraceEvent>>,
    next_seq: AtomicU64,
}

impl TraceCollector {
    /// Records one event. `start`/`end` are `chrono::Utc::now()` calls the
    /// caller took around the operation being recorded — this never calls
    /// the clock itself, so a caller that already has a timestamp (a
    /// streamed round's outcome, say) never needs a second one.
    pub(crate) fn record(
        &self,
        operation: &'static str,
        label: impl Into<String>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        attributes: Map<String, Value>,
    ) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        // Saturating rather than allowed to go negative: a caller's `end`
        // can never precede its own `start` in practice, but nothing here
        // depends on that being true — `num_milliseconds` producing a
        // negative value from clock skew should show up as 0, not as a
        // misleadingly precise negative duration.
        let duration_ms = (end - start).num_milliseconds().max(0);
        self.events
            .lock()
            .expect("trace collector lock should not be poisoned")
            .push(TraceEvent {
                seq,
                operation: operation.to_owned(),
                label: label.into(),
                start,
                end,
                duration_ms,
                attributes,
            });
    }

    /// A snapshot of every event recorded so far, ordered by `seq` (see
    /// `TraceEvent::seq`'s doc comment on why this sorts rather than
    /// returning insertion order directly).
    pub(crate) fn events(&self) -> Vec<TraceEvent> {
        let mut events = self
            .events
            .lock()
            .expect("trace collector lock should not be poisoned")
            .clone();
        events.sort_by_key(|event| event.seq);
        events
    }
}

/// Builds an attributes map from `(key, value)` pairs — a thin convenience
/// over `serde_json::Map`'s own builder-less API, used by every
/// `TraceCollector::record` call site instead of each repeating `Map::new()`
/// plus a chain of `.insert`.
pub(crate) fn attrs<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// Writes `events` to `path` as JSONL (one [`TraceEvent`] per line),
/// creating any missing parent directories. A single one-shot
/// `storage::write_atomic` call rather than an append-as-recorded writer:
/// every event is already buffered in memory for the whole run (see this
/// module's doc comment), so there is nothing an incremental writer would
/// buy that isn't already true of `checkpoint::save`'s own atomic
/// whole-file writes.
pub(crate) fn write_jsonl(path: &std::path::Path, events: &[TraceEvent]) -> Result<()> {
    let mut body = Vec::new();
    for event in events {
        serde_json::to_writer(&mut body, event).context("failed to serialize a trace event")?;
        body.push(b'\n');
    }
    storage::write_atomic(path, &body)
        .with_context(|| format!("failed to write trace file '{}'", path.display()))
}

/// Reads a [`write_jsonl`]-written trace file back, sorted by `seq`. Blank
/// lines are skipped; any other line that doesn't parse as a
/// [`TraceEvent`] is an error naming its line number.
pub(crate) fn read_jsonl(path: &std::path::Path) -> Result<Vec<TraceEvent>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read trace file '{}'", path.display()))?;
    let mut events = Vec::new();
    for (line_number, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: TraceEvent = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse trace event on line {} of '{}'",
                line_number + 1,
                path.display(),
            )
        })?;
        events.push(event);
    }
    events.sort_by_key(|event| event.seq);
    Ok(events)
}

/// One label's totals, for `lait trace show --summary`.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct LabelSummary {
    pub(crate) label: String,
    pub(crate) chat: u64,
    pub(crate) execute_tool: u64,
    pub(crate) compact: u64,
    pub(crate) duration_ms: i64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
}

/// Totals `events` per label, in the order each label first appears.
pub(crate) fn summarize(events: &[TraceEvent]) -> Vec<LabelSummary> {
    let mut summaries: Vec<LabelSummary> = Vec::new();
    for event in events {
        let index = match summaries
            .iter()
            .position(|summary| summary.label == event.label)
        {
            Some(index) => index,
            None => {
                summaries.push(LabelSummary {
                    label: event.label.clone(),
                    ..LabelSummary::default()
                });
                summaries.len() - 1
            }
        };
        let summary = &mut summaries[index];
        match event.operation.as_str() {
            "chat" => summary.chat += 1,
            "execute_tool" => summary.execute_tool += 1,
            "compact" => summary.compact += 1,
            _ => {}
        }
        summary.duration_ms = summary.duration_ms.saturating_add(event.duration_ms);
        let tokens = |key: &str| event.attributes.get(key).and_then(Value::as_u64);
        summary.input_tokens = summary
            .input_tokens
            .saturating_add(tokens("gen_ai.usage.input_tokens").unwrap_or_default());
        summary.output_tokens = summary
            .output_tokens
            .saturating_add(tokens("gen_ai.usage.output_tokens").unwrap_or_default());
    }
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_totals_counts_durations_and_tokens_per_label() {
        let collector = TraceCollector::default();
        let start = Utc::now();
        let end = start + chrono::Duration::milliseconds(10);
        collector.record(
            "chat",
            "step_a",
            start,
            end,
            attrs([
                ("gen_ai.usage.input_tokens", Value::from(5)),
                ("gen_ai.usage.output_tokens", Value::from(2)),
            ]),
        );
        collector.record("execute_tool", "tool 'x'", start, end, Map::new());
        collector.record(
            "chat",
            "step_a",
            start,
            end,
            attrs([("gen_ai.usage.input_tokens", Value::from(7))]),
        );
        let summaries = summarize(&collector.events());
        assert_eq!(summaries.len(), 2);
        assert_eq!(
            summaries[0],
            LabelSummary {
                label: "step_a".to_owned(),
                chat: 2,
                duration_ms: 20,
                input_tokens: 12,
                output_tokens: 2,
                ..LabelSummary::default()
            }
        );
        assert_eq!(summaries[1].execute_tool, 1);
    }

    #[test]
    fn events_are_returned_in_sequence_order_regardless_of_insertion_order() {
        let collector = TraceCollector::default();
        let now = Utc::now();
        // Record three events; `record` assigns `seq` in call order, so
        // reading them back should reproduce that order even if nothing
        // reorders them here — this pins the ordering guarantee `events()`
        // otherwise only demonstrates under real concurrency.
        collector.record("chat", "a", now, now, Map::new());
        collector.record("chat", "b", now, now, Map::new());
        collector.record("execute_tool", "c", now, now, Map::new());

        let events = collector.events();
        let labels: Vec<&str> = events.iter().map(|event| event.label.as_str()).collect();
        assert_eq!(labels, vec!["a", "b", "c"]);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[2].seq, 2);
    }

    #[test]
    fn duration_is_computed_from_start_and_end() {
        let collector = TraceCollector::default();
        let start = Utc::now();
        let end = start + chrono::Duration::milliseconds(42);
        collector.record("chat", "step", start, end, Map::new());
        assert_eq!(collector.events()[0].duration_ms, 42);
    }

    #[test]
    fn write_jsonl_round_trips_through_serde() {
        use crate::test_support::TempDir;

        let collector = TraceCollector::default();
        let now = Utc::now();
        collector.record(
            "chat",
            "step-one",
            now,
            now,
            attrs([("gen_ai.request.model", Value::from("test-model"))]),
        );
        let events = collector.events();

        let dir = TempDir::new("lait-trace");
        let path = dir.path().join("nested").join("trace.jsonl");
        write_jsonl(&path, &events).expect("write_jsonl should succeed");

        let contents = std::fs::read_to_string(&path).expect("trace file should exist");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: TraceEvent =
            serde_json::from_str(lines[0]).expect("trace event should round-trip through JSON");
        assert_eq!(parsed.label, "step-one");
        assert_eq!(
            parsed.attributes.get("gen_ai.request.model"),
            Some(&Value::from("test-model"))
        );
    }
}
