//! The shared tail every `run_*` subcommand reaches once it has produced a
//! finished response body: `-o`/`--render` output routing (currently
//! `run_chat`'s own concern — see the design plan's B-2 for extending these
//! flags to `run_prompt`/`run_agent`/`run_workflow`), `lait history`
//! recording (unless `--no-history`/`default.history: false` opts out), and
//! the `--show-usage` summary. Chat has its own richer version of the
//! record/summary half, `app::finish_chat_turn`, which also appends to a
//! `--session` log — this module's [`finish_run`] is for the three `run_*`
//! entry points that don't have a session concept.

use std::path::Path;

use anyhow::{Context, Result};

use crate::{
    config::ConfigFile,
    history, render, response,
    usage::{self, UsageTally},
};

/// Writes `body` to stdout — Markdown-rendered when `render_enabled` — or to
/// `output_path` verbatim with a trailing newline. `output_path`/
/// `render_enabled` are `None`/`false` from every caller but `run_chat`'s
/// non-streamed path until B-2 extends `-o`/`--render` to the rest, which
/// reduces this to a plain `println!`.
pub(crate) fn emit_output(
    body: &str,
    output_path: Option<&Path>,
    render_enabled: bool,
) -> Result<()> {
    match output_path {
        Some(path) => {
            let mut written = body.to_owned();
            written.push('\n');
            std::fs::write(path, written)
                .with_context(|| format!("failed to write the response to '{}'", path.display()))
        }
        None => {
            println!("{}", render::maybe_render(body, render_enabled));
            Ok(())
        }
    }
}

/// Records a completed chat/agent/workflow/prompt run in `lait history`,
/// unless `no_history` (the caller's own `--no-history`) or
/// `default.history: false` opts out — the one gate every `run_*` entry
/// point goes through before ever calling `history::record`, so recording
/// can never happen from a place that forgot to check the opt-out. Called
/// only after a run has actually succeeded (every call site is on the
/// success path), matching `history::record`'s own contract.
pub(crate) fn record_history(
    no_history: bool,
    file_config: &ConfigFile,
    kind: &str,
    model: Option<&str>,
    prompt: &str,
    response: &str,
    usage: Option<response::Usage>,
) -> Result<()> {
    if no_history || !file_config.default.history.unwrap_or(true) {
        return Ok(());
    }
    history::record(kind, model, prompt, response, usage)
}

/// What [`finish_run`] records: `record_history`'s `kind`/`model`/`prompt`/
/// `response`, bundled so `finish_run` itself doesn't grow a lint-dodging
/// argument count as this tail picks up more callers.
pub(crate) struct RunRecord<'a> {
    pub(crate) kind: &'a str,
    pub(crate) model: Option<&'a str>,
    pub(crate) prompt: &'a str,
    pub(crate) response: &'a str,
}

/// The `run_prompt`/`run_agent`/`run_workflow` tail: records this run (see
/// [`record_history`]) and prints the usage summary when asked.
pub(crate) fn finish_run(
    record: RunRecord<'_>,
    no_history: bool,
    file_config: &ConfigFile,
    usage_tally: &UsageTally,
    show_usage: bool,
) -> Result<()> {
    record_history(
        no_history,
        file_config,
        record.kind,
        record.model,
        record.prompt,
        record.response,
        usage_tally.total(),
    )?;
    if show_usage {
        usage::print_usage_summary(usage_tally);
    }
    Ok(())
}
