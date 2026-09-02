//! The shared tail every `run_*` subcommand reaches once it has produced a
//! finished response body: `-o`/`--render`/`--json` output routing
//! ([`emit_output`] for chat's own richer response-object path,
//! [`emit_run_output`] for `run_prompt`/`run_agent`/`run_workflow`'s plain-text
//! path), `lait history` recording (unless `--no-history`/
//! `default.history: false` opts out), and the `--show-usage` summary. Chat
//! has its own richer version of the record/summary half,
//! `app::finish_chat_turn`, which also appends to a `--session` log — this
//! module's [`finish_run`] is for the three `run_*` entry points that don't
//! have a session concept.

use std::path::Path;

use anyhow::{Context, Result};

use crate::{
    config::ConfigFile,
    history, render, response,
    usage::{self, UsageTally},
};

/// Writes `body` to stdout — Markdown-rendered when `render_enabled` — or to
/// `output_path` verbatim with a trailing newline. The chat streamed path
/// (`app::run_chat`) never reaches here — see `engine::stream_response` for
/// its own `-o` handling — every other caller goes through here, either
/// directly (chat's non-streamed path) or via [`emit_run_output`]
/// (`run_prompt`/`run_agent`/`run_workflow`). The file branch writes directly via
/// `std::fs::write` rather than through `async_io::write_output_file` (the
/// cancellable, path-locked primitive workflow's `write_file` node and
/// `execute_step`'s retry path use): this is a single, already-complete
/// response body written once outside any step's `timeout`, so there is no
/// cancellation deadline or concurrent-write race here for that primitive to
/// guard against.
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

/// Writes a `run_prompt`/`run_agent`/`run_workflow` response body per
/// `-o`/`--render`/`--json` (`cli::OutputArgs`, extended to these three entry
/// points by the design plan's B-2). `--json`'s shape
/// (`response::render_text_json`) matches chat's own `--json`, so the flag
/// means the same thing everywhere it appears; `--render` is ignored when
/// combined with `--json`, matching [`emit_output`]'s chat behavior.
pub(crate) fn emit_run_output(
    body: &str,
    usage: Option<response::Usage>,
    output: &crate::cli::OutputArgs,
    file_config: &ConfigFile,
) -> Result<()> {
    // `-o -` is an explicit "stdout", the same as no `-o` at all.
    let output_path = output
        .output
        .as_deref()
        .filter(|path| path.as_os_str() != "-");
    let render_enabled = output.render || file_config.default.render.unwrap_or(false);
    if output.json {
        let json = response::render_text_json(body, usage)?;
        emit_output(&json, output_path, false)
    } else {
        emit_output(body, output_path, render_enabled)
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
