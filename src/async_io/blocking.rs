//! The cancellable-blocking-worker primitive every other file in this module
//! builds on: [`run_blocking`] (and the pool it draws from), the per-output-
//! path lease [`run_blocking_with_path_lock`] adds on top, and
//! [`await_cancellation`] (racing an arbitrary future against cancellation,
//! not tied to a worker thread at all but living here since it is the same
//! "cancellation wins the race" primitive one level up). See this module's
//! parent doc comment for why a dedicated OS thread is used instead of
//! `tokio::spawn_blocking`.

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use std::io;

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

/// A filesystem operation gets one dedicated OS thread, rather than occupying
/// Tokio's shared blocking pool.  Keep the number of such threads bounded,
/// though: a caller can provide a large attachment list and a slow filesystem
/// can otherwise turn that list into an unbounded thread count.
pub(super) const MAX_BLOCKING_WORKERS: usize = 32;

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
pub(super) const BLOCKING_WORKER_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(100);

/// The single wording for a blocking I/O operation cancelled while waiting
/// for a worker slot ([`acquire_worker`]) or while running on one
/// ([`run_blocking_with_pool`], at both the pre-check and the `select!`
/// branch).
const BLOCKING_IO_CANCELLED: &str = "blocking I/O was cancelled";

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
pub(super) async fn run_blocking_with_pool<T, F>(
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
