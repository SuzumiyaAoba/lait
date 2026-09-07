//! Verbose logging / request tracing: `-v`/`-vv` and `LAIT_LOG`.
//!
//! `init` is called once, right after `Cli::parse()` in `main`, before any
//! command runs. Everything goes to stderr — never stdout, which must stay
//! pipe-clean for a piped `lait` answer — and ANSI color is disabled
//! whenever stderr isn't a terminal (a redirected log file, CI). This is
//! independent of the CLI's progress and error messages, which remain visible
//! regardless of the selected tracing level.

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

/// Initializes the global `tracing` subscriber. `LAIT_LOG` (a standard
/// `tracing_subscriber::EnvFilter` directive string, e.g. `debug` or
/// `lait=trace,reqwest=info`) wins when set, the same precedence an
/// env-var/flag pair like `LLM_MODEL`/`--model` uses elsewhere in this crate.
/// Otherwise `verbosity` (`-v`'s `ArgAction::Count`) selects a level scoped to
/// this crate only, so third-party dependency logs don't flood `-v` output:
/// `0` is silent, `1` (`-v`) is `debug`, `2+` (`-vv`) is `trace`.
pub(crate) fn init(verbosity: u8) {
    let filter = match std::env::var("LAIT_LOG") {
        Ok(value) if !value.trim().is_empty() => {
            EnvFilter::try_new(&value).unwrap_or_else(|_| default_filter(verbosity))
        }
        _ => default_filter(verbosity),
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

fn default_filter(verbosity: u8) -> EnvFilter {
    let directive = match verbosity {
        0 => "off",
        1 => "lait=debug",
        _ => "lait=trace",
    };
    EnvFilter::new(directive)
}
