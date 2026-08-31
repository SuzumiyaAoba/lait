//! `--show-usage` accounting: a per-run tally of every completion request's
//! token usage, and the summary printed from it. Extracted out of
//! `app.rs`'s `RunEnv`, which holds one [`UsageTally`] per run and is the
//! only thing that constructs or reads it.

use crate::response;

/// Accumulates every completion request's usage over one `lait` run, by the
/// label of whatever drove it (a workflow step, an agent, "chat"), so
/// `--show-usage` can print a per-label and total summary once a run
/// finishes. Lives in `app::RunEnv`, so recording must tolerate concurrent
/// callers (`parallel`/concurrent `for_each` steps record from concurrently
/// running tasks).
#[derive(Default)]
pub(crate) struct UsageTally {
    events: std::sync::Mutex<Vec<(String, response::Usage)>>,
    /// The running sum of every event recorded so far, kept incrementally so
    /// `total()` never has to refold `events` — `run_repl_turn` calls it
    /// twice per REPL turn purely to compute that turn's own delta, so an
    /// O(n) refold there would make an n-turn session cost O(n²) overall.
    running_total: std::sync::Mutex<Option<response::Usage>>,
}

impl UsageTally {
    /// Records `usage` under `label`.
    pub(crate) fn record(&self, label: &str, usage: response::Usage) {
        self.events
            .lock()
            .expect("usage tally lock should not be poisoned")
            .push((label.to_owned(), usage));
        let mut running_total = self
            .running_total
            .lock()
            .expect("usage tally lock should not be poisoned");
        let mut total = running_total.unwrap_or_default();
        total.add(usage);
        *running_total = Some(total);
    }

    /// Records `response`'s usage under `label`; a no-op when the server
    /// reported none (absence stays distinguishable from zero in the
    /// summary).
    pub(crate) fn record_response(&self, label: &str, response: &response::ChatCompletionResponse) {
        if let Some(usage) = response.usage {
            self.record(label, usage);
        }
    }

    /// Aggregates recorded events per label, in first-recorded order, as
    /// `(label, summed usage, request count)`.
    fn summarize(&self) -> Vec<(String, response::Usage, usize)> {
        let events = self
            .events
            .lock()
            .expect("usage tally lock should not be poisoned");
        let mut per_label: Vec<(String, response::Usage, usize)> = Vec::new();
        for (label, usage) in events.iter() {
            match per_label
                .iter_mut()
                .find(|(existing, _, _)| existing == label)
            {
                Some((_, sum, count)) => {
                    sum.add(*usage);
                    *count += 1;
                }
                None => per_label.push((label.clone(), *usage, 1)),
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
}

/// Prints the `--show-usage` summary to stderr: one line for a single-label
/// run (chat, a lone agent), a per-label breakdown plus total for a
/// workflow. Usage counts every request made under a label — a tool loop's
/// rounds, retries, and subagent calls (recorded under their own label) all
/// count toward what the run actually consumed.
pub(crate) fn print_usage_summary(tally: &UsageTally) {
    let per_label = tally.summarize();
    if per_label.is_empty() {
        eprintln!("usage: (the server reported no usage)");
        return;
    }
    let mut total = response::Usage::default();
    let mut requests = 0usize;
    for (_, usage, count) in &per_label {
        total.add(*usage);
        requests += count;
    }
    if per_label.len() == 1 {
        eprintln!("usage: {total}{}", requests_suffix(requests));
        return;
    }
    eprintln!("usage:");
    for (label, usage, count) in &per_label {
        eprintln!("  {label}: {usage}{}", requests_suffix(*count));
    }
    eprintln!("  total: {total}{}", requests_suffix(requests));
}

fn requests_suffix(count: usize) -> String {
    if count > 1 {
        format!(" ({count} requests)")
    } else {
        String::new()
    }
}
