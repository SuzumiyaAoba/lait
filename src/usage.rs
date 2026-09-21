//! `--show-usage` accounting: a per-run tally of every completion request's
//! token usage (and, when the resolved model has `pricing:` configured, its
//! estimated USD cost), and the summary printed from it. Extracted out of
//! `engine::RunContext`, which holds one [`UsageTally`] per run and is the
//! only thing that constructs or reads it.

use crate::{config::Pricing, response};

/// Accumulates every completion request's usage (and cost, when priced) over
/// one `lait` run, by the label of whatever drove it (a workflow step, an
/// agent, "chat"), so `--show-usage` can print a per-label and total summary
/// once a run finishes. Lives in `engine::RunContext`, so recording must
/// tolerate concurrent callers (`parallel`/concurrent `for_each` steps record
/// from concurrently running tasks).
#[derive(Default)]
pub(crate) struct UsageTally {
    events: std::sync::Mutex<Vec<(String, response::Usage, Option<f64>)>>,
    /// The running sum of every event recorded so far, kept incrementally so
    /// `total()` never has to refold `events` — `repl::run_turn` calls it
    /// twice per REPL turn purely to compute that turn's own delta, so an
    /// O(n) refold there would make an n-turn session cost O(n²) overall.
    running_total: std::sync::Mutex<Option<response::Usage>>,
    /// The running sum of every event's cost, when it had one — `None` until
    /// at least one priced event has been recorded (the same None-vs-zero
    /// convention `response::Usage`/`Pricing` themselves use), so a run with
    /// no `pricing:` configured anywhere never prints a misleadingly precise
    /// "$0.00".
    running_cost: std::sync::Mutex<Option<f64>>,
}

impl UsageTally {
    /// Records `usage` under `label`, at `pricing`'s rates (`None` when the
    /// resolved model has no `pricing:` configured — cost then stays
    /// unknown for this event, not zero).
    pub(crate) fn record(&self, label: &str, usage: response::Usage, pricing: Option<Pricing>) {
        let cost = pricing.map(|pricing| pricing.cost(usage));
        self.events
            .lock()
            .expect("usage tally lock should not be poisoned")
            .push((label.to_owned(), usage, cost));

        let mut running_total = self
            .running_total
            .lock()
            .expect("usage tally lock should not be poisoned");
        let mut total = running_total.unwrap_or_default();
        total.add(usage);
        *running_total = Some(total);

        if let Some(cost) = cost {
            let mut running_cost = self
                .running_cost
                .lock()
                .expect("usage tally lock should not be poisoned");
            *running_cost = Some(running_cost.unwrap_or(0.0) + cost);
        }
    }

    /// Records `response`'s usage under `label` at `pricing`'s rates; a
    /// no-op when the server reported none (absence stays distinguishable
    /// from zero in the summary).
    pub(crate) fn record_response(
        &self,
        label: &str,
        response: &response::ChatCompletionResponse,
        pricing: Option<Pricing>,
    ) {
        if let Some(usage) = response.usage {
            self.record(label, usage, pricing);
        }
    }

    /// Aggregates recorded events per label, in first-recorded order, as
    /// `(label, summed usage, request count, summed cost)`. A label's cost is
    /// `None` when none of its events were priced (rather than `Some(0.0)`).
    fn summarize(&self) -> Vec<(String, response::Usage, usize, Option<f64>)> {
        let events = self
            .events
            .lock()
            .expect("usage tally lock should not be poisoned");
        let mut per_label: Vec<(String, response::Usage, usize, Option<f64>)> = Vec::new();
        for (label, usage, cost) in events.iter() {
            match per_label
                .iter_mut()
                .find(|(existing, ..)| existing == label)
            {
                Some((_, sum, count, total_cost)) => {
                    sum.add(*usage);
                    *count += 1;
                    if let Some(cost) = cost {
                        *total_cost = Some(total_cost.unwrap_or(0.0) + cost);
                    }
                }
                None => per_label.push((label.clone(), *usage, 1, *cost)),
            }
        }
        per_label
    }

    /// The sum of every event recorded so far, across every label —
    /// `lait history`'s per-run `usage` field (see `app::record_history`).
    /// `None` when nothing has been recorded yet (never zero-vs-absent
    /// ambiguity, same convention as `response::Usage`'s own optionality).
    pub(crate) fn total(&self) -> Option<response::Usage> {
        *self
            .running_total
            .lock()
            .expect("usage tally lock should not be poisoned")
    }

    /// The sum of every priced event's cost recorded so far. `None` when
    /// nothing priced has been recorded (either nothing was recorded at all,
    /// or every recorded model has no `pricing:` configured) — see this
    /// struct's `running_cost` doc comment.
    pub(crate) fn total_cost(&self) -> Option<f64> {
        *self
            .running_cost
            .lock()
            .expect("usage tally lock should not be poisoned")
    }
}

/// Formats a USD amount the way every `--show-usage`/`lait compare` display
/// spells one, e.g. `$0.0023` — 4 decimal places, since per-request cost at
/// typical per-1M-token rates is often well under a cent and a 2-decimal
/// `$0.00` would round every such request down to the same misleading value.
pub(crate) fn format_cost(cost: f64) -> String {
    format!("${cost:.4}")
}

/// Prints the `--show-usage` summary to stderr: one line for a single-label
/// run (chat, a lone agent), a per-label breakdown plus total for a
/// workflow. Usage counts every request made under a label — a tool loop's
/// rounds, retries, and subagent calls (recorded under their own label) all
/// count toward what the run actually consumed. Cost is appended in
/// parentheses only when at least one recorded model has `pricing:`
/// configured; a run with none never mentions cost at all.
pub(crate) fn print_usage_summary(tally: &UsageTally) {
    let per_label = tally.summarize();
    if per_label.is_empty() {
        eprintln!("usage: (the server reported no usage)");
        return;
    }
    let mut total = response::Usage::default();
    let mut requests = 0usize;
    let mut total_cost: Option<f64> = None;
    for (_, usage, count, cost) in &per_label {
        total.add(*usage);
        requests += count;
        if let Some(cost) = cost {
            total_cost = Some(total_cost.unwrap_or(0.0) + cost);
        }
    }
    if per_label.len() == 1 {
        eprintln!(
            "usage: {total}{}{}",
            requests_suffix(requests),
            cost_suffix(total_cost)
        );
        return;
    }
    eprintln!("usage:");
    for (label, usage, count, cost) in &per_label {
        eprintln!(
            "  {label}: {usage}{}{}",
            requests_suffix(*count),
            cost_suffix(*cost)
        );
    }
    eprintln!(
        "  total: {total}{}{}",
        requests_suffix(requests),
        cost_suffix(total_cost)
    );
}

fn requests_suffix(count: usize) -> String {
    if count > 1 {
        format!(" ({count} requests)")
    } else {
        String::new()
    }
}

fn cost_suffix(cost: Option<f64>) -> String {
    match cost {
        Some(cost) => format!(" ({})", format_cost(cost)),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::UsageTally;
    use crate::{config::Pricing, response::Usage};

    const PRICING: Pricing = Pricing {
        input_per_1m: 1.0,
        output_per_1m: 2.0,
    };

    #[test]
    fn total_cost_is_none_when_nothing_was_recorded() {
        let tally = UsageTally::default();
        assert_eq!(tally.total_cost(), None);
    }

    #[test]
    fn total_cost_stays_none_when_no_recorded_event_was_priced() {
        let tally = UsageTally::default();
        tally.record(
            "step",
            Usage {
                prompt_tokens: 1_000_000,
                completion_tokens: 1_000_000,
                total_tokens: 2_000_000,
            },
            None,
        );
        assert_eq!(tally.total_cost(), None);
    }

    #[test]
    fn total_cost_accumulates_only_priced_events() {
        let tally = UsageTally::default();
        // 1M prompt + 1M completion tokens at $1/$2 per 1M => $3.00.
        tally.record(
            "priced",
            Usage {
                prompt_tokens: 1_000_000,
                completion_tokens: 1_000_000,
                total_tokens: 2_000_000,
            },
            Some(PRICING),
        );
        // Unpriced event contributes tokens to `total()` but not to cost.
        tally.record(
            "unpriced",
            Usage {
                prompt_tokens: 500,
                completion_tokens: 500,
                total_tokens: 1_000,
            },
            None,
        );
        assert_eq!(tally.total_cost(), Some(3.0));
        assert_eq!(tally.total().unwrap().total_tokens, 2_000_000 + 1_000);
    }

    #[test]
    fn format_cost_uses_four_decimal_places() {
        assert_eq!(super::format_cost(0.0023), "$0.0023");
    }
}
