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
/// wired" value. One shared `static` suffices because flag consumers only
/// ever read it.
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
