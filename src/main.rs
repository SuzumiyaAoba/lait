use async_openai::error::OpenAIError;
use clap::Parser;

mod agent;
mod app;
mod async_io;
mod attachment;
mod cli;
mod config;
mod docgen;
mod dotenv;
mod engine;
mod frontmatter;
mod history;
mod init;
mod jq;
mod jsonl;
mod lint;
mod llm;
mod mcp;
mod models;
mod nesting;
mod process;
mod prompt;
mod render;
mod repl;
mod report;
mod response;
mod schema;
mod session;
mod skill;
mod subagent;
mod template;
#[cfg(test)]
mod test_support;
mod usage;
mod workflow;

fn main() {
    // `.env` must be loaded before `Cli::parse()` runs (clap's `env = ...`
    // fallbacks read the process environment at parse time), so `--no-env`
    // is detected from the raw command line here; the `Cli` flag of the
    // same name only exists for `--help` and validation. Everything up to a
    // literal `--` is scanned, matching where clap itself would accept the
    // flag.
    let no_env = std::env::args()
        .skip(1)
        .take_while(|argument| argument != "--")
        .any(|argument| argument == "--no-env");
    if !no_env {
        // SAFETY: called before the tokio runtime below spawns its worker
        // threads, so no other thread can be reading the environment yet —
        // see `load_from_current_dir`'s safety contract.
        if let Err(error) = unsafe { dotenv::load_from_current_dir() } {
            exit_with_error(error, false);
        }
    }

    let cli = cli::Cli::parse();
    // Captured before `cli` is moved into `run_blocking`/`run` below — the
    // one classification `classify_error` can't reliably do from the
    // rendered message alone (see `ExitKind::Validation`'s doc).
    let is_lint = matches!(cli.command, Some(cli::Command::Lint(_)));

    // The purely local subcommands (completions/man/init/lint/local models)
    // never await; skip spawning the runtime's worker threads for them —
    // `lait completions` runs from shell startup files, where that cost is
    // felt on every new shell.
    if !app::needs_async_runtime(&cli) {
        if let Err(error) = app::run_blocking(cli) {
            exit_with_error(error, is_lint);
        }
        return;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => exit_with_error(
            anyhow::Error::new(error).context("failed to start the async runtime"),
            is_lint,
        ),
    };
    if let Err(error) = runtime.block_on(app::run(cli)) {
        exit_with_error(error, is_lint);
    }
}

/// The exit codes the design plan's B-3 defines. 0 (success) and 2 (a clap
/// usage/parse error, or `--help`/`--version`) never reach this type: clap's
/// own `Cli::parse()` above already exits with those before this file's code
/// ever sees a `Result`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitKind {
    /// Anything not classified below.
    General = 1,
    /// A `lait lint` finding, or a workflow/agent file that failed to parse
    /// (`serde_yaml::Error` in the error chain — the same type
    /// `workflow::parse_workflow`/`agent::load_agent` already propagate via
    /// `?`, so this needs no source changes to detect).
    Validation = 3,
    /// An HTTP/auth/rate-limit failure from the model endpoint.
    /// `async_openai::error::OpenAIError` already flows through
    /// `llm::complete`/`complete_stream`/the streamed-chunk path untouched,
    /// so downcasting for it costs nothing extra at the call sites either.
    ModelApi = 4,
    /// A step/request timeout, or a cancelled run (Ctrl+C).
    Interrupted = 5,
}

/// Classifies `error` for [`ExitKind`]. `is_lint` is passed separately
/// (`main`'s only caller checks `cli.command` before it's moved into
/// `run_blocking`/`run`) because a `lait lint` failure is `Validation` by
/// definition, regardless of what its message says — the other three
/// non-`General` kinds below are detected from `error` itself.
///
/// Timeout/cancellation (`async_io.rs`/`mcp.rs`/`llm.rs`/`workflow/exec.rs`,
/// around forty call sites) are still plain `anyhow!`/`bail!` strings, not a
/// typed error — giving every one of them a dedicated type is a bigger,
/// separate refactor than this lightweight classifier justifies (see the
/// design plan's own "no `thiserror` rewrite" call for B-3). They all
/// consistently say "cancelled" or "timed out", so `Interrupted` is
/// detected from the rendered message instead; a future message that stops
/// saying either of those falls back to `General` rather than
/// misclassifying as something worse.
fn classify_error(error: &anyhow::Error, is_lint: bool) -> ExitKind {
    if is_lint {
        return ExitKind::Validation;
    }
    if error.chain().any(|cause| cause.is::<OpenAIError>()) {
        return ExitKind::ModelApi;
    }
    let rendered = format!("{error:#}");
    if rendered.contains("cancelled") || rendered.contains("timed out") {
        return ExitKind::Interrupted;
    }
    if error.chain().any(|cause| cause.is::<serde_yaml::Error>()) {
        return ExitKind::Validation;
    }
    ExitKind::General
}

fn exit_with_error(error: anyhow::Error, is_lint: bool) -> ! {
    eprintln!("lait: {error:#}");
    std::process::exit(classify_error(&error, is_lint) as i32);
}
