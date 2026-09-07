//! Process-wide Ctrl-C (SIGINT) handling for every genuinely single-shot
//! async command — `lait run`, single-shot chat, `lait agent run`,
//! `lait prompt run` — each arms `spawn_handler` before its initial
//! config/input read. `repl::run`'s multi-turn REPL owns the listener for its
//! whole loop.
//!
//! The first Ctrl-C cancels the process's root cancellation token
//! token, which every in-flight blocking I/O op, model request, and MCP
//! call is already wired to react to (see `async_io.rs`) — including a
//! workflow's own `run_steps` step loop
//! (`workflow::exec::check_workflow_cancellation`), whose failing step is
//! then caught by `app::run_workflow`'s per-top-level-step loop the same way
//! any other step failure is, saving a `--checkpoint` (if enabled) before
//! returning. A second Ctrl-C hard-exits immediately, for the case where
//! cleanup itself is stuck (e.g. a child process ignoring its own
//! termination signal).

use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

/// The conventional shell exit code for a process terminated by SIGINT
/// (128 + signal number 2) — see `main::exit_with_error`.
pub(crate) const SIGINT_EXIT_CODE: i32 = 130;

static RECEIVED: AtomicBool = AtomicBool::new(false);

/// Whether this process has seen at least one Ctrl-C. Consulted by
/// `main::exit_with_error` to give a user-initiated cancellation its own
/// exit code instead of folding it into `ExitKind::Interrupted`'s generic 5
/// (which a step's own `timeout:` — cancelled the same way, but never a
/// user interrupt — still gets).
pub(crate) fn received() -> bool {
    RECEIVED.load(Ordering::Relaxed)
}

/// Spawns the process-wide Ctrl-C listener onto the current Tokio runtime.
/// Call once, before running the actual command, so a second signal is
/// armed from the very start rather than only after the first one lands.
pub(crate) fn spawn_handler(token: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            // No SIGINT handler could be installed (an unsupported
            // platform/environment) — nothing to watch for.
            return;
        }
        RECEIVED.store(true, Ordering::Relaxed);
        eprintln!("\nlait: received Ctrl-C, stopping (press Ctrl-C again to exit immediately)...");
        token.cancel();
        // A second Ctrl-C hard-exits without waiting for cleanup, in case
        // that cleanup itself is stuck.
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("lait: received a second Ctrl-C; exiting immediately");
        std::process::exit(SIGINT_EXIT_CODE);
    });
}
