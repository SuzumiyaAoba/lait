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
    io::{self, Read},
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
                bail!(
                    "file contents exceed the configured read limit of {} bytes",
                    self.limit
                );
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
    acquire_permit(
        path_lock(path),
        cancellation,
        "output path is still owned by a previous write",
    )
    .await
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
            bail!("blocking I/O was cancelled");
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
        bail!("blocking I/O was cancelled");
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
            bail!("blocking I/O was cancelled");
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
        bail!("file read was cancelled");
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
        let fifo_path = file_type.is_fifo().then_some(path);
        read_from_file(
            &mut file,
            file_type.is_fifo() && wait_for_fifo_writer,
            fifo_path,
            cancelled,
            max_bytes,
            budget,
        )
    }

    #[cfg(not(unix))]
    let mut file = File::open(path)?;

    #[cfg(not(unix))]
    read_from_file(&mut file, false, None, cancelled, max_bytes, budget)
}

/// Reads a regular or non-blocking special file. A FIFO with no writer would
/// otherwise report EOF immediately when opened non-blocking. When the caller
/// requests writer-waiting, poll the descriptor so an empty FIFO is returned
/// after a writer opens and closes, just as a blocking FIFO read would be.
/// The cancellation flag is checked on every poll interval.
fn read_from_file(
    file: &mut File,
    wait_for_fifo_writer: bool,
    fifo_path: Option<&Path>,
    cancelled: &AtomicBool,
    max_bytes: usize,
    budget: &ReadBudget,
) -> Result<Vec<u8>> {
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut contents = Vec::new();
    // `poll` reports POLLHUP on a non-blocking FIFO while no writer exists.
    // Keep that state separate from the post-connection EOF state: after a
    // writer has been observed, an empty FIFO must finish with EOF exactly as
    // a blocking read would.
    let mut fifo_writer_seen = !wait_for_fifo_writer;
    let mut buffer = [0_u8; CHUNK_SIZE];

    loop {
        if cancelled.load(Ordering::Acquire) {
            bail!("file read was cancelled");
        }

        #[cfg(unix)]
        if wait_for_fifo_writer && !fifo_writer_seen {
            match wait_for_fifo_event(file, fifo_path.expect("FIFO path is required"))? {
                FifoEvent::NoWriter => continue,
                FifoEvent::WriterConnected => fifo_writer_seen = true,
                FifoEvent::Data(byte) => {
                    if contents.len() >= max_bytes {
                        bail!(
                            "file contents exceed the configured read limit of {} bytes",
                            max_bytes
                        );
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
                if wait_for_fifo_writer && !fifo_writer_seen && contents.is_empty() {
                    continue;
                }
                return Ok(contents);
            }
            Ok(read) if contents.len() >= max_bytes => {
                let _ = read;
                bail!(
                    "file contents exceed the configured read limit of {} bytes",
                    max_bytes
                );
            }
            Ok(read) => {
                if read > max_bytes - contents.len() {
                    bail!(
                        "file contents exceed the configured read limit of {} bytes",
                        max_bytes
                    );
                }
                budget.claim(read)?;
                contents.extend_from_slice(&buffer[..read]);
            }
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
    loop {
        // A short poll interval lets the cancellation check in the caller
        // run even while no FIFO writer exists. `poll` itself is bounded and
        // therefore cannot recreate the old uninterruptible worker problem.
        let result = unsafe { libc::poll(&mut pollfd, 1, 10) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                pollfd.revents = 0;
                continue;
            }
            return Err(anyhow::anyhow!(
                "polling FIFO '{}' failed: {error}",
                path.display()
            ));
        }

        // POSIX does not expose a FIFO's writer count. In particular, macOS
        // reports no poll event for both "no writer" and "writer connected,
        // no data". Temporarily opening a non-blocking write descriptor and
        // then probing the reader distinguishes those states without leaving
        // a synthetic writer attached: EOF means the probe was the only
        // writer, WouldBlock means a real writer is still connected, and a
        // byte is retained for the normal read loop.
        return probe_fifo_writer(file, path);
    }
}

#[cfg(unix)]
fn probe_fifo_writer(file: &mut File, path: &Path) -> Result<FifoEvent> {
    use std::os::unix::fs::OpenOptionsExt;

    match OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(probe) => drop(probe),
        Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
            return Ok(FifoEvent::NoWriter);
        }
        Err(error) => return Err(error.into()),
    }

    let mut byte = [0_u8; 1];
    match file.read(&mut byte) {
        Ok(0) => {
            // The probe was the only writer. Avoid a tight loop while waiting
            // for a real writer to arrive (poll can return immediately for a
            // FIFO in this state on some platforms).
            std::thread::sleep(Duration::from_millis(10));
            Ok(FifoEvent::NoWriter)
        }
        Ok(1) => Ok(FifoEvent::Data(byte[0])),
        Ok(read) => bail!("FIFO probe read an unexpected number of bytes: {read}"),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(FifoEvent::WriterConnected),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => probe_fifo_writer(file, path),
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

#[cfg(test)]
mod tests {
    use super::{
        ReadBudget, acquire_path_lock, read_file, read_file_wait_for_fifo_writer,
        run_blocking_with_path_lock, run_blocking_with_pool,
    };
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };
    use tokio_util::sync::CancellationToken;

    fn test_worker_pool() -> Arc<tokio::sync::Semaphore> {
        Arc::new(tokio::sync::Semaphore::new(super::MAX_BLOCKING_WORKERS))
    }

    #[tokio::test]
    async fn cancellation_cleanup_does_not_wait_for_an_uncooperative_worker() {
        let token = CancellationToken::new();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = Arc::clone(&started);
        let task = tokio::spawn(run_blocking_with_pool(
            move |_| {
                worker_started.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            },
            Some(token.clone()),
            test_worker_pool(),
        ));

        let wait_started = Instant::now();
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        token.cancel();
        let result = task.await.unwrap();

        assert!(result.is_err());
        assert!(
            wait_started.elapsed() < Duration::from_millis(400),
            "cancellation waited for the worker: {:?}",
            wait_started.elapsed()
        );
    }

    #[tokio::test]
    async fn saturated_worker_acquisition_recovers_after_workers_release() {
        let mut workers = Vec::new();
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let worker_pool = test_worker_pool();

        for _ in 0..super::MAX_BLOCKING_WORKERS {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            workers.push(tokio::spawn(run_blocking_with_pool(
                move |_| {
                    started.fetch_add(1, Ordering::AcqRel);
                    while !release.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(())
                },
                None,
                Arc::clone(&worker_pool),
            )));
        }

        let wait_started = Instant::now();
        while started.load(Ordering::Acquire) < super::MAX_BLOCKING_WORKERS {
            tokio::task::yield_now().await;
        }
        let token = CancellationToken::new();
        let result = run_blocking_with_pool(move |_| Ok(()), Some(token), Arc::clone(&worker_pool));
        let result = tokio::time::timeout(
            super::BLOCKING_WORKER_ACQUIRE_TIMEOUT + Duration::from_millis(100),
            result,
        )
        .await
        .expect("a saturated worker pool must not wait forever")
        .unwrap_err();
        assert!(result.to_string().contains("saturated"));
        assert!(wait_started.elapsed() < Duration::from_secs(2));

        // The failed acquisition must not consume or permanently lose a
        // permit. Once the existing workers finish, a new operation should
        // be admitted and run normally.
        release.store(true, Ordering::Release);
        for worker in workers {
            worker.await.unwrap().unwrap();
        }

        let ran = Arc::new(AtomicBool::new(false));
        let worker_ran = Arc::clone(&ran);
        let result = tokio::time::timeout(
            super::BLOCKING_WORKER_ACQUIRE_TIMEOUT + Duration::from_millis(100),
            run_blocking_with_pool(
                move |_| {
                    worker_ran.store(true, Ordering::Release);
                    Ok(42_u8)
                },
                None,
                Arc::clone(&worker_pool),
            ),
        )
        .await
        .expect("a released worker permit must admit a subsequent operation")
        .unwrap();
        assert_eq!(result, 42);
        assert!(ran.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn an_already_cancelled_operation_does_not_spawn_a_worker() {
        let token = CancellationToken::new();
        token.cancel();
        let ran = Arc::new(AtomicBool::new(false));
        let worker_ran = Arc::clone(&ran);

        let result = run_blocking_with_pool(
            move |_| {
                worker_ran.store(true, Ordering::Release);
                Ok(())
            },
            Some(token),
            test_worker_pool(),
        )
        .await;

        assert!(result.is_err());
        assert!(
            !ran.load(Ordering::Acquire),
            "an already-cancelled operation must stop before spawning a worker"
        );
    }

    #[tokio::test]
    async fn a_path_lease_stays_with_an_uncooperative_worker_after_cancellation() {
        let path = crate::test_support::unique_temp_path("lait-test-path-lease", ".out");
        let token = CancellationToken::new();
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let worker_started = Arc::clone(&started);
        let worker_finished = Arc::clone(&finished);
        let worker_path = path.clone();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            run_blocking_with_path_lock(
                &worker_path,
                move |_| {
                    // This models a network/FUSE write that ignores the
                    // cooperative cancellation flag while the kernel call is
                    // in progress. The injected closure makes ownership
                    // behavior deterministic without relying on a particular
                    // filesystem.
                    worker_started.store(true, Ordering::Release);
                    std::thread::sleep(Duration::from_millis(500));
                    worker_finished.store(true, Ordering::Release);
                    Ok(())
                },
                Some(task_token),
            )
            .await
        });

        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        token.cancel();
        let cancelled = tokio::time::timeout(Duration::from_millis(250), task)
            .await
            .expect("cancellation cleanup must remain bounded")
            .unwrap();
        assert!(cancelled.is_err());
        assert!(!finished.load(Ordering::Acquire));

        let retry_token = CancellationToken::new();
        let retry = tokio::time::timeout(
            super::BLOCKING_WORKER_ACQUIRE_TIMEOUT + Duration::from_millis(100),
            acquire_path_lock(&path, Some(&retry_token)),
        )
        .await
        .expect("a retry must not wait indefinitely for a stuck writer")
        .unwrap_err();
        assert!(retry.to_string().contains("previous write"));

        // Once the injected worker returns, its closure drops the lease and
        // the same path can be admitted normally again.
        let deadline = Instant::now() + Duration::from_secs(1);
        while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(finished.load(Ordering::Acquire));
        let retry_token = CancellationToken::new();
        let _permit = acquire_path_lock(&path, Some(&retry_token)).await.unwrap();
    }

    #[tokio::test]
    async fn blocking_workers_are_limited() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let maximum = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = Vec::new();
        let worker_pool = test_worker_pool();

        for _ in 0..(super::MAX_BLOCKING_WORKERS * 2) {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            tasks.push(tokio::spawn(run_blocking_with_pool(
                move |_| {
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum.fetch_max(current, Ordering::AcqRel);
                    std::thread::sleep(Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::AcqRel);
                    Ok(())
                },
                None,
                Arc::clone(&worker_pool),
            )));
        }

        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert!(
            maximum.load(Ordering::Acquire) <= super::MAX_BLOCKING_WORKERS,
            "too many blocking workers ran concurrently: {}",
            maximum.load(Ordering::Acquire)
        );
    }

    #[test]
    fn read_file_rejects_bytes_beyond_the_explicit_limit() {
        let path = crate::test_support::unique_temp_path("lait-test-read-limit", ".txt");
        fs::write(&path, b"1234").unwrap();
        let cancelled = AtomicBool::new(false);

        let error = read_file(&path, &cancelled, 3).unwrap_err();
        assert!(error.to_string().contains("read limit"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_shared_read_budget_caps_the_combined_materialized_bytes() {
        let first = crate::test_support::unique_temp_path("lait-test-read-budget", "-first.txt");
        let second = crate::test_support::unique_temp_path("lait-test-read-budget", "-second.txt");
        fs::write(&first, b"123").unwrap();
        fs::write(&second, b"456").unwrap();
        let cancelled = AtomicBool::new(false);
        let budget = ReadBudget::new(5);

        assert_eq!(
            super::read_file_with_budget(&first, &cancelled, 5, &budget, false).unwrap(),
            b"123"
        );
        let error =
            super::read_file_with_budget(&second, &cancelled, 5, &budget, false).unwrap_err();
        assert!(error.to_string().contains("read limit"));
        let _ = fs::remove_file(first);
        let _ = fs::remove_file(second);
    }

    #[cfg(unix)]
    #[test]
    fn empty_fifo_is_eof_without_waiting_for_a_writer_by_default() {
        let path = crate::test_support::unique_temp_path("lait-test-empty-fifo", "");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        let result = read_file(&path, &cancelled, 16).unwrap();

        assert!(result.is_empty());
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "empty FIFO did not return EOF promptly: {:?}",
            started.elapsed()
        );
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn waiting_for_a_fifo_writer_can_be_cancelled_before_a_writer_connects() {
        let path = crate::test_support::unique_temp_path("lait-test-fifo-cancel", "");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let token = CancellationToken::new();
        let worker_path = path.clone();
        let task = tokio::spawn(run_blocking_with_pool(
            move |cancelled| {
                read_file_wait_for_fifo_writer(&worker_path, cancelled, super::MAX_READ_BYTES)
            },
            Some(token.clone()),
            test_worker_pool(),
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("a FIFO read without a writer must react to cancellation")
            .unwrap();
        assert!(result.is_err());

        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn waiting_for_a_fifo_writer_returns_empty_eof_after_an_empty_writer_closes() {
        let path = crate::test_support::unique_temp_path("lait-test-fifo-empty", "");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let worker_path = path.clone();
        let task = tokio::spawn(run_blocking_with_pool(
            move |cancelled| {
                read_file_wait_for_fifo_writer(&worker_path, cancelled, super::MAX_READ_BYTES)
            },
            None,
            test_worker_pool(),
        ));

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            // Keep the writer open long enough for poll to observe the
            // transition from POLLHUP to the connected/no-data state.
            std::thread::sleep(Duration::from_millis(50));
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(writer_path)
                .expect("failed to open FIFO writer");
            std::thread::sleep(Duration::from_millis(50));
            drop(file);
        });

        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("an empty connected FIFO must eventually return EOF")
            .unwrap()
            .unwrap();
        writer.join().unwrap();
        assert!(result.is_empty());

        let _ = fs::remove_file(path);
    }
}
