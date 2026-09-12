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

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    future::Future,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

use crate::file_lock;

/// A filesystem operation gets one dedicated OS thread, rather than occupying
/// Tokio's shared blocking pool.  Keep the number of such threads bounded,
/// though: a caller can provide a large attachment list and a slow filesystem
/// can otherwise turn that list into an unbounded thread count.
const MAX_BLOCKING_WORKERS: usize = 32;

/// A worker is allowed a short grace period to notice cancellation and send
/// its result after the caller has signalled it. The worker itself may still
/// be stuck in an OS call (for example, a network filesystem), so waiting for
/// its oneshot beyond this deadline would defeat cancellation.
const BLOCKING_CLEANUP_TIMEOUT: Duration = Duration::from_millis(100);

/// A caller that tries to start another operation while every worker is stuck
/// in an uninterruptible system call must receive a bounded error instead of
/// waiting on the semaphore forever. A worker that eventually returns still
/// releases its permit, so this is a back-pressure timeout rather than a
/// permanent loss of capacity.
const BLOCKING_WORKER_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(100);

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

/// The single wording for a blocking I/O operation cancelled while waiting
/// for a worker slot ([`acquire_worker`]) or while running on one
/// ([`run_blocking_with_pool`], at both the pre-check and the `select!`
/// branch).
const BLOCKING_IO_CANCELLED: &str = "blocking I/O was cancelled";

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

fn blocking_workers() -> &'static Arc<tokio::sync::Semaphore> {
    static WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    WORKERS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_BLOCKING_WORKERS)))
}

/// A cancelled worker may still be inside an OS call after its async owner
/// has returned.  Writers therefore keep a per-path lease in addition to the
/// shared worker-pool permit: a retry of the same path cannot start until the
/// old worker has actually dropped its lease.  Weak values keep this map from
/// retaining every path ever written for the lifetime of the process.
fn path_locks() -> &'static Mutex<HashMap<PathBuf, Weak<tokio::sync::Semaphore>>> {
    static PATH_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Semaphore>>>> =
        OnceLock::new();
    PATH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn path_lock(path: &Path) -> Arc<tokio::sync::Semaphore> {
    let mut locks = path_locks()
        .lock()
        .expect("async I/O path lock registry should not be poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Semaphore::new(1));
    locks.insert(path.to_owned(), Arc::downgrade(&lock));
    lock
}

/// Acquires the ownership lease for an output path.  The lease must be moved
/// into the worker closure, not dropped by the async owner on cancellation;
/// this is what makes bounded cancellation cleanup safe when a network/FUSE
/// write ignores the cooperative flag for a while.
pub(crate) async fn acquire_path_lock(
    path: &Path,
    cancellation: Option<&CancellationToken>,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    let path = path.to_owned();
    let key = run_blocking(
        move |cancelled| output_path_identity(&path, cancelled),
        cancellation.cloned(),
    )
    .await?;
    acquire_permit(
        path_lock(&key),
        cancellation,
        "output path is still owned by a previous write",
    )
    .await
}

/// Resolve aliases before entering the lock table. Outputs need not exist yet;
/// in that case normalize their parent and also follow a dangling final symlink.
/// Filesystem resolution runs on the same bounded, cancellable worker pool as I/O.
fn output_path_identity(path: &Path, cancelled: &AtomicBool) -> Result<PathBuf> {
    let mut current = path.to_owned();
    let mut suffix = Vec::new();
    let mut links = 0;
    loop {
        if cancelled.load(Ordering::Acquire) {
            bail!(crate::error::cancelled(
                "output path resolution was cancelled"
            ));
        }
        match std::fs::canonicalize(&current) {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to resolve output path '{}'", path.display())
                });
            }
        }
        if std::fs::symlink_metadata(&current)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            if links >= 40 {
                bail!(
                    "too many symbolic links in output path '{}'",
                    path.display()
                );
            }
            links += 1;
            let target = std::fs::read_link(&current).with_context(|| {
                format!("failed to read output symlink '{}'", current.display())
            })?;
            current = if target.is_absolute() {
                target
            } else {
                current.parent().unwrap_or(Path::new(".")).join(target)
            };
            continue;
        }
        let name = current
            .file_name()
            .with_context(|| format!("invalid output path '{}'", path.display()))?
            .to_owned();
        suffix.push(name);
        current = current
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_owned();
    }
}

/// Runs a blocking operation while holding the ownership lease for `path`.
/// The lease is transferred into the worker closure before the common worker
/// admission point is entered. This small composition helper keeps callers
/// from accidentally dropping a path lease when a bounded cancellation
/// cleanup returns before an uncooperative worker has finished.
pub(crate) async fn run_blocking_with_path_lock<T, F>(
    path: &Path,
    operation: F,
    cancellation: Option<CancellationToken>,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&AtomicBool) -> Result<T> + Send + 'static,
{
    let lease = acquire_path_lock(path, cancellation.as_ref()).await?;
    run_blocking(
        move |cancelled| {
            let _lease = lease;
            operation(cancelled)
        },
        cancellation,
    )
    .await
}

async fn acquire_worker(
    semaphore: Arc<tokio::sync::Semaphore>,
    cancellation: Option<&CancellationToken>,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    acquire_permit(
        semaphore,
        cancellation,
        "blocking I/O worker limit is currently saturated",
    )
    .await
}

async fn acquire_permit(
    semaphore: Arc<tokio::sync::Semaphore>,
    cancellation: Option<&CancellationToken>,
    saturated_message: &'static str,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    let Some(cancellation) = cancellation else {
        return tokio::time::timeout(BLOCKING_WORKER_ACQUIRE_TIMEOUT, semaphore.acquire_owned())
            .await
            .map_err(|_| anyhow::anyhow!(saturated_message))?
            .context("blocking I/O permit owner was closed");
    };

    tokio::select! {
        biased;
        permit = semaphore.clone().acquire_owned() => {
            permit.context("blocking I/O permit owner was closed")
        }
        () = cancellation.cancelled() => {
            bail!(crate::error::cancelled(BLOCKING_IO_CANCELLED));
        }
        () = tokio::time::sleep(BLOCKING_WORKER_ACQUIRE_TIMEOUT) => {
            bail!("{saturated_message}");
        }
    }
}

/// Runs a blocking operation on a dedicated OS thread and observes an
/// optional workflow cancellation signal. The operation receives a flag it
/// must check between bounded I/O operations. If this future is dropped, the
/// guard also sets the flag, so a worker that outlives the future still gets a
/// chance to stop.
pub(crate) async fn run_blocking<T, F>(
    operation: F,
    cancellation: Option<CancellationToken>,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&AtomicBool) -> Result<T> + Send + 'static,
{
    run_blocking_with_pool(operation, cancellation, Arc::clone(blocking_workers())).await
}

/// Runs a blocking operation against a caller-owned worker pool.
///
/// Production callers use [`run_blocking`], whose semaphore is shared so all
/// timeout-sensitive filesystem work has one bounded admission point. Tests
/// that intentionally occupy every permit use this helper with a private
/// semaphore; that keeps a saturation scenario deterministic without starving
/// unrelated tests which happen to run in parallel in the same process.
async fn run_blocking_with_pool<T, F>(
    operation: F,
    cancellation: Option<CancellationToken>,
    worker_pool: Arc<tokio::sync::Semaphore>,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&AtomicBool) -> Result<T> + Send + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let permit = acquire_worker(worker_pool, cancellation.as_ref()).await?;

    if cancellation
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        drop(permit);
        bail!(crate::error::cancelled(BLOCKING_IO_CANCELLED));
    }

    let (sender, mut receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("lait-blocking-io".to_owned())
        .spawn(move || {
            let _permit = permit;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                operation(&worker_cancelled)
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("blocking I/O worker panicked")));
            let _ = sender.send(result);
        })
        .context("failed to spawn blocking I/O worker")?;

    let guard = CancellationGuard {
        cancelled: Arc::clone(&cancelled),
        armed: true,
    };

    let Some(cancellation) = cancellation else {
        let result = receiver
            .await
            .context("blocking I/O worker was cancelled")??;
        drop(guard);
        return Ok(result);
    };

    // `biased` polls the cancellation branch first every turn, so it always
    // wins a tie against an already-ready `receiver` — unlike a
    // `watch::Receiver`'s edge-triggered `changed()`, `cancelled()` stays
    // ready forever once cancelled, so there's no need to re-check it inside
    // the `receiver` arm the way the old watch-channel version had to.
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            cancel_worker(&cancelled, &mut receiver).await;
            bail!(crate::error::cancelled(BLOCKING_IO_CANCELLED));
        }
        result = &mut receiver => {
            let result = result.context("blocking I/O worker was cancelled")??;
            drop(guard);
            Ok(result)
        }
    }
}

async fn cancel_worker<T>(
    cancelled: &AtomicBool,
    receiver: &mut tokio::sync::oneshot::Receiver<Result<T>>,
) {
    cancelled.store(true, Ordering::Release);
    let _ = tokio::time::timeout(BLOCKING_CLEANUP_TIMEOUT, receiver).await;
}

struct CancellationGuard {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

/// The outcome of [`await_cancellation`]: `future` either finished on its
/// own, or `cancellation` fired first. Kept as an explicit variant (rather
/// than folding straight into an error) because some callers need to react
/// differently to a cancellation than to an ordinary failure — `mcp::call`
/// evicts the exact cached connection a cancelled request was using, which a
/// plain `Err` couldn't distinguish from a normal protocol error.
pub(crate) enum CancellationResult<T> {
    Completed(T),
    Cancelled,
}

/// Awaits `future` while racing it against `cancellation`, so a caller driving
/// several cancellable operations at once (an LLM request alongside MCP/
/// subagent work, say) can stop as soon as any of them is told to. Merely
/// dropping `future` on a timeout is not enough by itself: something has to
/// actually poll a shared cancellation signal for every such operation to
/// notice it at the same time, which is exactly what this does.
///
/// If `future` and `cancellation` both become ready in the same poll,
/// cancellation wins — a timeout handler may already have started a cleanup/
/// retry sequence by the time the `future` branch is checked, and returning
/// its result here would let a caller believe an attempt that's already being
/// torn down elsewhere completed normally.
pub(crate) async fn await_cancellation<F, T>(
    future: F,
    cancellation: Option<CancellationToken>,
) -> CancellationResult<T>
where
    F: Future<Output = T>,
{
    let Some(cancellation) = cancellation else {
        return CancellationResult::Completed(future.await);
    };

    tokio::select! {
        biased;
        () = cancellation.cancelled() => CancellationResult::Cancelled,
        result = future => CancellationResult::Completed(result),
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
        use std::os::unix::fs::OpenOptionsExt;

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

#[cfg(unix)]
enum FifoEvent {
    NoWriter,
    WriterConnected,
    Data(u8),
}

#[cfg(unix)]
fn wait_for_fifo_event(file: &mut File, path: &Path) -> Result<FifoEvent> {
    use std::os::fd::AsRawFd;

    let mut pollfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN | libc::POLLERR,
        revents: 0,
    };
    // A short poll interval lets the cancellation check in the caller run
    // even while no FIFO writer exists. `poll` itself is bounded and therefore
    // cannot recreate the old uninterruptible worker problem.
    let result = unsafe { libc::poll(&mut pollfd, 1, 10) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            // Return to the outer read loop so it can observe the shared
            // cancellation flag before polling again. Re-entering this loop
            // would let a signal storm postpone cancellation indefinitely.
            return Ok(FifoEvent::NoWriter);
        }
        return Err(anyhow::anyhow!(
            "polling FIFO '{}' failed: {error}",
            path.display()
        ));
    }

    // On systems exposing POLLHUP, this also observes a writer that connected
    // and closed without leaving any bytes between our polls.
    if pollfd.revents & libc::POLLHUP != 0 {
        return Ok(FifoEvent::WriterConnected);
    }
    probe_fifo_reader(file)
}

#[cfg(unix)]
fn probe_fifo_reader(file: &mut File) -> Result<FifoEvent> {
    // Reading an empty nonblocking FIFO returns EOF when no writer exists,
    // and WouldBlock when a writer is connected. Do not manufacture a writer:
    // that changes FIFO state, requires write permission, and its descriptor
    // can be inherited transiently by concurrent process creation.
    let mut byte = [0_u8; 1];
    match file.read(&mut byte) {
        Ok(0) => {
            std::thread::sleep(Duration::from_millis(10));
            Ok(FifoEvent::NoWriter)
        }
        Ok(1) => Ok(FifoEvent::Data(byte[0])),
        Ok(read) => bail!("FIFO probe read an unexpected number of bytes: {read}"),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(FifoEvent::WriterConnected),
        // Return to the outer cancellation check instead of recursively
        // probing under a sustained stream of signals.
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(FifoEvent::NoWriter),
        Err(error) => Err(error.into()),
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
    read_to_string(path, &AtomicBool::new(false), MAX_READ_BYTES)
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
/// for a FIFO writer only when a cancellation channel is present (a caller
/// with no channel has no way to be told to give up on one, so there is
/// nothing to wait for). Shared by every loader (agent files, skills, JSON
/// schemas) that reads exactly one file and returns its contents as a string.
pub(crate) async fn read_to_string_cancellable(
    path: &Path,
    cancellation: Option<CancellationToken>,
    max_bytes: usize,
) -> Result<String> {
    let path = path.to_owned();
    let wait_for_fifo_writer = cancellation.is_some();
    run_blocking(
        move |cancelled| {
            if wait_for_fifo_writer {
                read_to_string_wait_for_fifo_writer(&path, cancelled, max_bytes)
            } else {
                read_to_string(&path, cancelled, max_bytes)
            }
        },
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
pub(crate) async fn canonicalize(
    path: &Path,
    cancellation: Option<CancellationToken>,
) -> Result<PathBuf> {
    let path = path.to_owned();
    run_blocking(move |_| Ok(std::fs::canonicalize(path)?), cancellation).await
}

/// Writes a workflow node's output from a dedicated OS thread. The worker is
/// kept off Tokio's runtime because a write to a special file such as a FIFO
/// can block indefinitely. A timeout sets the worker's cancellation flag and
/// waits for it to finish; Unix special files are opened non-blocking so that
/// this cleanup cannot itself get stuck. Regular files use the same direct
/// create/truncate/write behavior as `fs::write`, with cancellation checks
/// between bounded chunks so existing inode, permission, hard-link, and
/// symlink semantics remain intact. After opening a regular file, an advisory
/// descriptor lease from [`crate::file_lock`] is held through the write so
/// hard-link aliases and path replacement after canonicalization are serialized
/// when the cooperating writers use the same lease. External writers that do
/// not take an advisory lease remain outside this guarantee.
pub(crate) async fn write_output_file(
    path: &Path,
    output: &str,
    step_cancel: Option<CancellationToken>,
) -> Result<()> {
    let path = path.to_owned();
    let output = output.to_owned();
    // `run_blocking_with_path_lock` deliberately returns after a bounded
    // cancellation cleanup even when an OS/network filesystem call ignores
    // the cancellation flag; transferring the lease to the worker prevents a
    // retry from writing the same path concurrently with that still-running
    // worker.
    let worker_path = path.clone();
    run_blocking_with_path_lock(
        &path,
        move |cancelled| {
            write_output_file_blocking(&worker_path, &output, cancelled)
                .with_context(|| format!("failed to write output to '{}'", worker_path.display()))
        },
        step_cancel,
    )
    .await
}

/// Performs the blocking half of [`write_output_file`]. On Unix, the target is
/// opened once with `O_NONBLOCK` and classified from that same handle. This
/// removes the metadata-then-open TOCTOU window while preserving symlink,
/// inode, permission, and hard-link behavior for regular files. FIFOs and
/// other non-regular files continue through non-blocking I/O. Other platforms
/// reject non-regular handles after a conservative path preflight, rather than
/// attempting to write a device, named pipe, or reparse point.
fn write_output_file_blocking(path: &Path, output: &str, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!(crate::error::cancelled("output file write was cancelled"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = loop {
            if cancelled.load(Ordering::Acquire) {
                bail!(crate::error::cancelled("output file write was cancelled"));
            }
            match OpenOptions::new()
                .write(true)
                .create(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
            {
                Ok(file) => break file,
                // Opening a FIFO for writing without a reader reports ENXIO
                // when O_NONBLOCK is set. Poll until a reader appears or the
                // workflow cancellation flag asks us to stop.
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        };

        if !file.metadata()?.file_type().is_file() {
            return write_nonblocking_special_file(file, output, cancelled);
        }

        // The canonical path lease above serializes aliases we can resolve by
        // name. This second, advisory descriptor lease covers hard links and
        // symlink replacement after path resolution. It is acquired before
        // truncate and held through the entire regular-file write.
        let _lease = file_lock::ExclusiveLease::acquire(&file, cancelled)?;
        write_regular_output_file(&mut file, output, cancelled)
    }

    #[cfg(not(unix))]
    {
        // Windows has no portable non-blocking File API. Reject an already
        // visible special/reparse target before opening it, then repeat the
        // check on the opened handle to keep a path swap from turning into a
        // write to a device or named pipe. Symlinks to regular files retain
        // the existing follow-and-overwrite behavior.
        match std::fs::metadata(path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                bail!(
                    "refusing to write non-regular output path '{}'",
                    path.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut file = OpenOptions::new().write(true).create(true).open(path)?;
        if !file.metadata()?.file_type().is_file() {
            bail!(
                "refusing to write non-regular output path '{}'",
                path.display()
            );
        }
        // Keep the descriptor lease on the opened regular file until the
        // write returns; this also covers every failure path before/after
        // truncation without relying on the path name remaining stable.
        let _lease = file_lock::ExclusiveLease::acquire(&file, cancelled)?;
        write_regular_output_file(&mut file, output, cancelled)
    }
}

/// Writes an ordinary file directly, preserving the target inode and the
/// overwrite/permission behavior of `fs::write`. Chunking only exists to give
/// a timed worker a bounded opportunity to observe cancellation.
fn write_regular_output_file(file: &mut File, output: &str, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!(crate::error::cancelled("output file write was cancelled"));
    }
    // Truncate only after the handle has been classified as a regular file.
    // A timeout after this point intentionally leaves an empty/partial file:
    // direct truncation is what preserves the existing inode, permissions,
    // hard links, and symlink-following semantics of `fs::write`, but it is
    // not an atomic replacement. The caller receives an error and must not
    // treat the partial bytes as a completed node output.
    file.set_len(0)?;
    for chunk in output.as_bytes().chunks(64 * 1024) {
        if cancelled.load(Ordering::Acquire) {
            bail!(crate::error::cancelled("output file write was cancelled"));
        }
        file.write_all(chunk)?;
    }
    file.flush()?;
    if cancelled.load(Ordering::Acquire) {
        bail!(crate::error::cancelled("output file write was cancelled"));
    }
    Ok(())
}

#[cfg(unix)]
/// Writes FIFOs and other Unix special files with non-blocking I/O. Opening
/// the descriptor with `O_NONBLOCK` by the caller means no system
/// call can hold the worker past cancellation. The same handle is used for
/// classification and writing; reopening the path here would reintroduce a
/// metadata/open TOCTOU race.
fn write_nonblocking_special_file(
    mut file: File,
    output: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    let bytes = output.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if cancelled.load(Ordering::Acquire) {
            bail!(crate::error::cancelled("output file write was cancelled"));
        }
        match file.write(&bytes[offset..]) {
            Ok(0) => bail!("output file write made no progress"),
            Ok(written) => offset += written,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_writable(&file)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    if cancelled.load(Ordering::Acquire) {
        bail!(crate::error::cancelled("output file write was cancelled"));
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_writable(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;

    let mut pollfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLOUT | libc::POLLERR | libc::POLLHUP,
        revents: 0,
    };
    // Keep the poll bounded so the caller can re-check its cancellation flag
    // between waits. When the reader drains the FIFO, POLLOUT wakes this
    // worker immediately instead of adding another fixed sleep.
    let result = unsafe { libc::poll(&mut pollfd, 1, 10) };
    if result >= 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted {
        // Return to the writer loop so it can check cancellation before
        // attempting another write. This keeps the poll wait bounded even
        // when signals repeatedly interrupt poll(2).
        return Ok(());
    }
    Err(error).context("polling output file for writability failed")
}

#[cfg(test)]
mod tests;
