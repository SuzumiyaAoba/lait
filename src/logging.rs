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
///
/// Skips building and installing a subscriber at all when there is nothing
/// to log (`verbosity == 0` and `LAIT_LOG` unset/blank): every `tracing`
/// macro is a no-op without one installed, so this changes nothing about
/// what a caller sees, only whether `main` pays for an `EnvFilter` parse and
/// a `stderr().is_terminal()` check on every invocation — `lait completions`
/// in particular runs from shell startup files (see `main`'s own comment on
/// why that path's cost matters), where this was pure overhead before.
pub(crate) fn init(verbosity: u8) {
    let log_env = std::env::var("LAIT_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if verbosity == 0 && log_env.is_none() {
        return;
    }

    let filter = match log_env {
        Some(value) => EnvFilter::try_new(&value).unwrap_or_else(|error| {
            eprintln!(
                "lait: warning: invalid LAIT_LOG directive {value:?} ({error}); falling back to -v/-vv"
            );
            default_filter(verbosity)
        }),
        None => default_filter(verbosity),
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
