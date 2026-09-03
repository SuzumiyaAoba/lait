//! Verbose logging / request tracing: `-v`/`-vv` and `LAIT_LOG`.
//!
//! `init` is called once, right after `Cli::parse()` in `main`, before any
//! command runs. Everything goes to stderr — never stdout, which must stay
//! pipe-clean for a piped `lait` answer — and ANSI color is disabled
//! whenever stderr isn't a terminal (a redirected log file, CI). This is
//! purely additive: the 48 pre-existing `eprintln!` call sites across the
//! crate (`lait: `/`note: `/`warning: `/`==> ` prefixes) are untouched and
//! keep behaving exactly as before, with or without `-v`.

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

/// Masks a secret (an API key) for a log line: keeps the first 4 characters —
/// enough to tell two configured keys apart without revealing enough to be
/// useful — and replaces the rest with `***`. Fewer than 4 characters masks
/// entirely, so a short/placeholder key (e.g. the `"lm-studio"` dummy
/// `engine::resolve_request_settings` substitutes) doesn't leak in full.
pub(crate) fn mask_secret(secret: &str) -> String {
    if secret.chars().count() < 4 {
        return "***".to_owned();
    }
    let prefix: String = secret.chars().take(4).collect();
    format!("{prefix}***")
}

#[cfg(test)]
mod tests {
    use super::mask_secret;

    #[test]
    fn masks_everything_but_the_first_four_characters() {
        assert_eq!(mask_secret("sk-1234567890"), "sk-1***");
    }

    #[test]
    fn masks_a_short_secret_entirely() {
        assert_eq!(mask_secret("abc"), "***");
        assert_eq!(mask_secret(""), "***");
    }
}
