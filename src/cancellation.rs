//! Shared cancellation plumbing: the typed-error check helpers both sides
//! of the cancellation channel spell the same way, plus the never-tripped
//! sentinels call sites use where no cancellation is wired.
//!
//! The crate's cancellation channel comes in two forms: a
//! [`CancellationToken`] on the async side (Tokio tasks, `select!` arms) and
//! a plain [`AtomicBool`] flag on the blocking-worker side (`async_io`
//! workers, which can't hold a token across an OS thread boundary and only
//! ever poll the flag between bounded operations). Both used to be wrapped
//! in `Option` at every boundary — `None` meant "no cancellation wired",
//! which is exactly what a parentless, never-cancelled token (or a flag that
//! is never set) already expresses, so the `Option` layer only added
//! `.clone()`/`is_some_and` noise. Call sites that genuinely want "do this
//! uncancelled" — `app::workflow_run`'s failure-path checkpoint save — pass
//! [`none()`]/[`&NEVER_SET`] and document why.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use tokio_util::sync::CancellationToken;

/// A flag that is never set: the blocking-worker side's "no cancellation
/// wired" value. One shared `static` suffices *only* because every current
/// consumer (`file_walk::DirWalker`, `async_io::read_to_string_sync`,
/// `jq::apply_bool`) only ever reads it. This is a contract callers must
/// keep, not something the type system enforces: `&'static AtomicBool` is
/// freely writable, and a future call site that `.store()`s into a borrowed
/// flag parameter — expecting to affect only its own caller — would instead
/// flip this single process-wide `static`, cancelling every other unrelated
/// sentinel consumer at once. `Option<&AtomicBool>::None` made this
/// unrepresentable in the type system; the sentinel form does not, so keep
/// treating every `&NEVER_SET` reference as read-only.
pub(crate) static NEVER_SET: AtomicBool = AtomicBool::new(false);

/// The token side's "no cancellation wired" value: a parentless token that
/// no call site cancels. `CancellationToken` is not const-constructible, so
/// unlike [`NEVER_SET`] this is a function returning a fresh token.
///
/// A caller threading this into a FIFO-waiting reader
/// (`async_io::read_to_string_cancellable`, `skill::load_skill`'s
/// `read_to_string_wait_for_fifo_writer` call) and then simply `.await`ing
/// the result to completion gets an unconditionally unbounded wait: nothing
/// will ever cancel this token, and nothing else is dropping the future
/// either. `app::doctor::run`'s MCP checks avoid this by racing their own
/// `tokio::time::timeout` around the call instead — dropping *that* future
/// on expiry is what actually bounds the wait, not this token. Passing
/// `none()` into a read path with no such outer bound reintroduces the exact
/// hang `run_blocking`'s cancel-on-drop guard exists to prevent.
pub(crate) fn none() -> CancellationToken {
    CancellationToken::new()
}

/// `token.is_cancelled()` → `Err(error::cancelled(message))` — the single
/// spelling for every async-side "check cancellation before doing work" site.
pub(crate) fn check(token: &CancellationToken, message: &'static str) -> Result<()> {
    if token.is_cancelled() {
        bail!(crate::error::cancelled(message));
    }
    Ok(())
}

/// `flag.load(Acquire)` → `Err(error::cancelled(message))` — the blocking-
/// worker counterpart of [`check`].
pub(crate) fn check_flag(flag: &AtomicBool, message: &'static str) -> Result<()> {
    if flag.load(Ordering::Acquire) {
        bail!(crate::error::cancelled(message));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{NEVER_SET, check, check_flag, none};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn check_passes_through_an_uncancelled_token() {
        assert!(check(&none(), "not cancelled").is_ok());
    }

    #[test]
    fn check_reports_a_cancelled_token_as_a_typed_interruption() {
        let token = none();
        token.cancel();
        let error = check(&token, "was cancelled").unwrap_err();
        assert!(crate::error::is_interrupted(&error));
        assert_eq!(error.to_string(), "was cancelled");
    }

    #[test]
    fn check_flag_passes_through_a_clear_flag() {
        assert!(check_flag(&NEVER_SET, "not cancelled").is_ok());
    }

    #[test]
    fn check_flag_reports_a_set_flag_as_a_typed_interruption() {
        let flag = AtomicBool::new(true);
        let error = check_flag(&flag, "was cancelled").unwrap_err();
        assert!(crate::error::is_interrupted(&error));
        assert_eq!(error.to_string(), "was cancelled");
    }

    #[test]
    fn never_set_never_reads_as_cancelled() {
        // `NEVER_SET` is a single process-wide `static` shared by every
        // caller that has no cancellation source wired — this only holds as
        // long as nothing ever stores into it (see the `static`'s own doc).
        assert!(!NEVER_SET.load(Ordering::Acquire));
    }

    #[test]
    fn none_returns_a_fresh_uncancelled_token_each_call() {
        let a = none();
        let b = none();
        assert!(!a.is_cancelled());
        assert!(!b.is_cancelled());
        // Cancelling one must not affect the other — `none()` hands out an
        // independent, parentless token per call, not a shared one.
        a.cancel();
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
    }
}
