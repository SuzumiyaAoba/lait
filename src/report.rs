//! The shared tail every `run_*` subcommand reaches once it has produced a
//! finished response body: `-o`/`--render`/`--json` output routing
//! ([`emit_output`] for chat's own richer response-object path,
//! [`emit_run_output`] for `run_prompt`/`run_agent`/`run_workflow`'s plain-text
//! path), `lait history` recording (unless `--no-history`/
//! `default.history: false` opts out), and the `--show-usage` summary. Chat
//! has its own richer version of the record/summary half,
//! `chat::finish_chat_turn`, which also appends to a `--session` log — this
//! module's [`finish_run`] is for the three `run_*` entry points that don't
//! have a session concept.

use std::{borrow::Cow, io::IsTerminal, path::Path, sync::LazyLock};

use anyhow::{Context, Result};

use crate::{
    config::ConfigFile,
    history, response,
    usage::{self, UsageTally},
};

/// `termimad::MadSkin::default()` is a fixed, stateless style table (no
/// per-render configuration ever varies it here), so it's built once and
/// shared instead of reconstructed on every rendered response.
/// `Send + Sync` holds because every field is either a `Copy` style/color
/// type or a `&'static` reference (`skin.rs`'s struct definition) — nothing
/// interior-mutable, so sharing one instance across renders is safe.
static SKIN: LazyLock<termimad::MadSkin> = LazyLock::new(termimad::MadSkin::default);

const _: fn() = || {
    fn assert_sync<T: Sync>() {}
    assert_sync::<termimad::MadSkin>();
};

/// Renders `content` as Markdown for terminal display (`--render`/
/// `default.render`, see docs/usage/ja/output.md) when `enabled` and stdout
/// is a terminal; otherwise returns `content` unchanged. Returns a borrow of
/// `content` in the common (disabled, or non-TTY) path instead of an owned
/// copy — [`emit_output`], its only caller, only ever needs to print the
/// result once, immediately, so there is nothing for the copy to buy.
fn maybe_render(content: &str, enabled: bool) -> Cow<'_, str> {
    if !enabled || !std::io::stdout().is_terminal() {
        return Cow::Borrowed(content);
    }
    Cow::Owned(SKIN.term_text(content).to_string())
}

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
            // Writes `body`'s bytes directly, then a trailing newline,
            // instead of `body.to_owned()` + `push('\n')` — a response body
            // can be sizeable, and that used to copy all of it just to
            // append one byte.
            use std::io::Write as _;
            (|| {
                let mut file = std::fs::File::create(path)?;
                file.write_all(body.as_bytes())?;
                file.write_all(b"\n")
            })()
            .with_context(|| format!("failed to write the response to '{}'", path.display()))
        }
        None => {
            println!("{}", maybe_render(body, render_enabled));
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

#[cfg(test)]
mod tests {
    use super::maybe_render;

    #[test]
    fn maybe_render_returns_content_unchanged_when_disabled() {
        // Also covers the enabled-but-not-a-terminal case: `cargo test`
        // runs with stdout captured (not a real TTY), so `enabled: true`
        // here exercises exactly that fallback too.
        assert_eq!(maybe_render("# Heading", false), "# Heading");
        assert_eq!(maybe_render("# Heading", true), "# Heading");
    }
}
