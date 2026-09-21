//! Cancellation-aware file reading: [`read_file`]/[`read_to_string`] and
//! their FIFO-waiting and shared-budget variants, plus the small path
//! utilities ([`canonicalize`], [`is_not_found`]) that belong to the same
//! timeout-sensitive loader paths. FIFO-specific polling lives in [`fifo`];
//! the cancellable-worker-thread primitive every blocking read runs on lives
//! in [`super::blocking`].

use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool, atomic::Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

use super::blocking::run_blocking;
#[cfg(unix)]
use super::fifo::{FifoEvent, wait_for_fifo_event};

/// Default upper bound for standalone file-backed inputs (agent files and JSON
/// schemas). Attachment callers use their smaller combined budget.
pub(crate) const MAX_READ_BYTES: usize = 16 * 1024 * 1024;

/// The single wording used everywhere a read in this module crosses its
/// configured byte limit (a shared [`ReadBudget`] running out, or a single
/// file's own `max_bytes` cap in [`read_from_file`]'s several check sites) —
/// mirrors `jq::limits::output_limit_exceeded_message`'s precedent for the
/// same shape of duplication.
fn read_limit_exceeded_message(byte_limit: usize) -> String {
    format!("file contents exceed the configured read limit of {byte_limit} bytes")
}

/// The single wording for a file read cancelled mid-read — see
/// [`read_file_with_budget`] and [`read_from_file`], its only two call
/// sites.
const FILE_READ_CANCELLED: &str = "file read was cancelled";

/// Bytes already materialized by a group of related reads. Sharing this
/// budget between concurrently-read attachments prevents each individual
/// worker from staying within its own limit while the combined `Vec`s still
/// grow without bound.
#[derive(Clone, Debug)]
pub(crate) struct ReadBudget {
    limit: usize,
    used: Arc<std::sync::atomic::AtomicUsize>,
}

impl ReadBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            used: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn claim(&self, bytes: usize) -> Result<()> {
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let next = used
                .checked_add(bytes)
                .ok_or_else(|| anyhow::anyhow!("file read size exceeded the configured limit"))?;
            if next > self.limit {
                bail!(read_limit_exceeded_message(self.limit));
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(current) => used = current,
            }
        }
    }
}

/// Reads a path completely, checking `cancelled` between bounded chunks.
/// `max_bytes` is enforced against bytes actually read, not just metadata, so
/// a growing regular file or a FIFO cannot make the returned `Vec` unbounded.
/// Regular files retain the normal `fs::read` follow-symlink behavior. On
/// Unix, FIFOs and other special files are opened with `O_NONBLOCK` so no
/// system call can remain blocked after a timeout.
pub(crate) fn read_file(path: &Path, cancelled: &AtomicBool, max_bytes: usize) -> Result<Vec<u8>> {
    read_file_with_budget(
        path,
        cancelled,
        max_bytes,
        &ReadBudget::new(max_bytes),
        false,
    )
}

/// Like [`read_file`], but preserves the old blocking-read behavior for a FIFO
/// that has not acquired a writer yet. The worker polls the non-blocking file
/// descriptor until a writer appears; a cancellation flag can interrupt that
/// wait. This keeps the non-cancellable attachment API compatible with the
/// old `fs::read` behavior without putting an uninterruptible `open` call in a
/// timed workflow worker.
pub(crate) fn read_file_wait_for_fifo_writer(
    path: &Path,
    cancelled: &AtomicBool,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    read_file_with_budget(
        path,
        cancelled,
        max_bytes,
        &ReadBudget::new(max_bytes),
        true,
    )
}

/// Reads using a shared byte budget. This is used by multiple concurrent file
/// attachments so their materialized contents cannot exceed one combined cap.
pub(crate) fn read_file_with_budget(
    path: &Path,
    cancelled: &AtomicBool,
    max_bytes: usize,
    budget: &ReadBudget,
    wait_for_fifo_writer: bool,
) -> Result<Vec<u8>> {
    if cancelled.load(Ordering::Acquire) {
        bail!(crate::error::cancelled(FILE_READ_CANCELLED));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

        // Open and classify the same descriptor.  A path can be replaced
        // between `metadata(path)` and a later `open(path)`, which would let
        // the type check describe one inode while the read consumes another.
        // `File::metadata` is fstat on Unix, so this preserves the decision
        // and the bytes under one handle while still allowing a FIFO to be
        // opened without a writer.
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        let file_type = file.metadata()?.file_type();
        // `Some`/`None` collapses the two states `read_from_file` used to
        // take as separate `bool`/`Option<&Path>` parameters that only ever
        // varied together (see that function's own doc comment).
        let fifo_wait_path = (file_type.is_fifo() && wait_for_fifo_writer).then_some(path);
        read_from_file(&mut file, fifo_wait_path, cancelled, max_bytes, budget)
    }

    #[cfg(not(unix))]
    let _ = wait_for_fifo_writer;

    #[cfg(not(unix))]
    let mut file = File::open(path)?;

    #[cfg(not(unix))]
    read_from_file(&mut file, None, cancelled, max_bytes, budget)
}

/// Reads a regular or non-blocking special file. A FIFO with no writer would
/// otherwise report EOF immediately when opened non-blocking. When
/// `fifo_wait_path` is `Some`, poll that path's descriptor so an empty FIFO
/// is returned only after a writer opens and closes, just as a blocking FIFO
/// read would be — `None` skips all of that and reads as an ordinary file.
/// The cancellation flag is checked on every poll interval.
///
/// `fifo_wait_path` used to be two separate parameters (`wait_for_fifo_writer:
/// bool` and `fifo_path: Option<&Path>`) that only ever varied together: the
/// caller could only reach `wait_for_fifo_writer: true` by first confirming
/// `path` names a FIFO, which is exactly when `fifo_path` was `Some`. That
/// left `fifo_path.expect("FIFO path is required")` below as the one place
/// in this crate proving an invariant the type system couldn't — collapsing
/// to one `Option` makes the invalid `(true, None)` state unrepresentable
/// instead.
fn read_from_file(
    file: &mut File,
    fifo_wait_path: Option<&Path>,
    cancelled: &AtomicBool,
    max_bytes: usize,
    budget: &ReadBudget,
) -> Result<Vec<u8>> {
    // Unlike the old `(wait_for_fifo_writer, fifo_path)` pair — where only
    // `fifo_path` needed suppressing on non-Unix, since `wait_for_fifo_writer`
    // was already read unconditionally below — `fifo_wait_path` alone is
    // read unconditionally by the same `Ok(0)` arm on every platform, so no
    // `#[cfg(not(unix))] let _ = ...;` is needed here.
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut contents = Vec::new();
    // A nonblocking reader can see EOF before any writer connects. Keep that
    // initial state separate from EOF after an observed writer or data, so
    // waiting reads do not incorrectly return an empty file at startup.
    #[cfg(unix)]
    let mut fifo_writer_seen = fifo_wait_path.is_none();
    #[cfg(not(unix))]
    let fifo_writer_seen = true;
    let mut buffer = [0_u8; CHUNK_SIZE];

    loop {
        if cancelled.load(Ordering::Acquire) {
            bail!(crate::error::cancelled(FILE_READ_CANCELLED));
        }

        #[cfg(unix)]
        if !fifo_writer_seen && let Some(fifo_path) = fifo_wait_path {
            match wait_for_fifo_event(file, fifo_path)? {
                FifoEvent::NoWriter => continue,
                FifoEvent::WriterConnected => fifo_writer_seen = true,
                FifoEvent::Data(byte) => {
                    if contents.len() >= max_bytes {
                        bail!(read_limit_exceeded_message(max_bytes));
                    }
                    budget.claim(1)?;
                    contents.push(byte);
                    fifo_writer_seen = true;
                }
            }
        }

        // Once the per-file limit is full, perform a one-byte probe. This
        // allows an exactly-at-limit file to succeed while rejecting a file
        // that grew after metadata was checked.
        let read_len = max_bytes.saturating_sub(contents.len()).min(CHUNK_SIZE);
        let read_len = if read_len == 0 { 1 } else { read_len };
        match file.read(&mut buffer[..read_len]) {
            Ok(0) => {
                // This branch is only reachable before a writer is observed
                // on platforms whose poll implementation can report EOF
                // during the no-writer state.  Do not turn that transient
                // HUP into a successful empty read.
                // Once bytes have been received, however, a writer may have
                // written and closed between polls; EOF is then final even
                // when the last poll also carried POLLHUP.
                if fifo_wait_path.is_some() && !fifo_writer_seen && contents.is_empty() {
                    continue;
                }
                return Ok(contents);
            }
            Ok(read) if contents.len() >= max_bytes => {
                let _ = read;
                bail!(read_limit_exceeded_message(max_bytes));
            }
            Ok(read) => {
                if read > max_bytes - contents.len() {
                    bail!(read_limit_exceeded_message(max_bytes));
                }
                budget.claim(read)?;
                contents.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Reads UTF-8 text through [`read_file`], preserving the path in the error
/// at the call site while keeping the worker itself independent of UI wording.
pub(crate) fn read_to_string(
    path: &Path,
    cancelled: &AtomicBool,
    max_bytes: usize,
) -> Result<String> {
    let bytes = read_file(path, cancelled, max_bytes)?;
    String::from_utf8(bytes).context("file contents were not valid UTF-8")
}

/// Synchronous counterpart to [`read_to_string_cancellable`], for a caller
/// with no async runtime to run on (`lint`, `agent list`/`prompt list`, and
/// other purely local commands — see `app::needs_async_runtime`). Applies
/// the same [`MAX_READ_BYTES`] bound and non-blocking Unix special-file
/// handling [`read_file`] gives every other reader in this module, rather
/// than a bare `std::fs::read_to_string`, so a sync-path caller and its
/// `_cancellable` counterpart enforce the same limits on the same kind of
/// file — the point of routing every sync reader through this one function
/// instead of each reaching for `std::fs::read_to_string` directly. Unlike
/// `read_to_string_cancellable`'s FIFO-waiting variant, this never waits for
/// a writer: a synchronous caller has no cancellation channel to interrupt
/// that wait, so a FIFO with no writer yet fails fast instead of hanging.
pub(crate) fn read_to_string_sync(path: &Path) -> Result<String> {
    read_to_string(path, &crate::cancellation::NEVER_SET, MAX_READ_BYTES)
}

/// FIFO-waiting counterpart to [`read_to_string`].
pub(crate) fn read_to_string_wait_for_fifo_writer(
    path: &Path,
    cancelled: &AtomicBool,
    max_bytes: usize,
) -> Result<String> {
    let bytes = read_file_wait_for_fifo_writer(path, cancelled, max_bytes)?;
    String::from_utf8(bytes).context("file contents were not valid UTF-8")
}

/// Reads UTF-8 text through the cancellation-aware blocking worker, waiting
/// for a FIFO writer to appear if needed — `run_blocking`'s guard trips the
/// flag on drop, so the wait can be interrupted once `cancellation` is
/// actually cancelled (or the caller drops this call's own future, e.g. by
/// racing it in a `tokio::time::timeout`). Passing `cancellation::none()` (a
/// token nothing ever cancels — see that function's doc) and then simply
/// `.await`ing this call to completion removes both of those exits: a FIFO
/// with no writer then blocks forever. Shared by every loader (agent files,
/// skills, JSON schemas) that reads exactly one file and returns its
/// contents as a string.
pub(crate) async fn read_to_string_cancellable(
    path: &Path,
    cancellation: CancellationToken,
    max_bytes: usize,
) -> Result<String> {
    let path = path.to_owned();
    run_blocking(
        move |cancelled| read_to_string_wait_for_fifo_writer(&path, cancelled, max_bytes),
        cancellation,
    )
    .await
}

/// Whether `error` (from one of this module's readers, wrapped in
/// `anyhow::Error` by `?` on the way up through a `with_context`) has a
/// missing-file `std::io::Error` anywhere in its cause chain. Every reader
/// here that treats "file doesn't exist" as its own outcome (an optional
/// config layer, a cache/cassette miss, ...) checks this once its read
/// fails, rather than pre-checking existence with a separate `is_file`/
/// `exists` call — a check-then-read has a race a direct read+classify does
/// not.
pub(crate) fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Resolves a path on a worker so cancellation cannot be delayed by a slow
/// network/FUSE filesystem. `canonicalize` is metadata I/O rather than a file
/// read, but it belongs to the same timeout-sensitive loader paths.
pub(crate) async fn canonicalize(path: &Path, cancellation: CancellationToken) -> Result<PathBuf> {
    let path = path.to_owned();
    run_blocking(move |_| Ok(std::fs::canonicalize(path)?), cancellation).await
}
