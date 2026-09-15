//! Cancellation-aware wrappers for filesystem operations used by workflow
//! execution.
//!
//! Tokio's `spawn_blocking` tasks cannot be stopped once they have started:
//! dropping the `JoinHandle` only stops waiting for the task. That is a poor
//! fit for workflow timeouts because a read from a FIFO (or a slow filesystem)
//! can keep the runtime's blocking pool occupied forever. The operations in
//! this module run on dedicated OS threads, use a shared cancellation flag,
//! and use non-blocking descriptors for Unix special files. A dropped future
//! still signals its worker through the guard's `Drop` implementation.
//!
//! Split into four files by concern: [`blocking`] (the cancellable-worker-
//! thread primitive everything else runs on), [`read`]/[`write`] (the actual
//! file operations), and [`fifo`] (Unix-only FIFO polling `read` uses) —
//! carving `fifo` out on its own makes visible, as a file boundary, that a
//! cancellable FIFO-aware read is needed by exactly this module's callers
//! and nowhere else in the crate (see AGENTS.md's note on not routing more
//! call sites through this module than actually need it).

mod blocking;
#[cfg(unix)]
mod fifo;
mod read;
mod write;

#[cfg(test)]
use blocking::{
    BLOCKING_WORKER_ACQUIRE_TIMEOUT, MAX_BLOCKING_WORKERS, acquire_path_lock,
    run_blocking_with_path_lock, run_blocking_with_pool,
};
pub(crate) use blocking::{CancellationResult, await_cancellation, run_blocking};
#[cfg(test)]
use read::read_file;
#[cfg(all(test, unix))]
use read::read_file_wait_for_fifo_writer;
pub(crate) use read::{
    MAX_READ_BYTES, ReadBudget, canonicalize, is_not_found, read_file_with_budget,
    read_to_string_cancellable, read_to_string_sync, read_to_string_wait_for_fifo_writer,
};
pub(crate) use write::write_output_file;

#[cfg(test)]
mod tests;
