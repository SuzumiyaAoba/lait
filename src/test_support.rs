//! Shared helpers for the `#[cfg(test)]` unit tests colocated in `src/`'s own
//! modules. Distinct from `tests/support`, which backs the integration tests
//! under `tests/` — those compile as a separate crate and can't reach this
//! one (or vice versa).

/// A path under the OS temp directory, unique to this process and call:
/// `{prefix}-{pid}-{nanos}-{counter}{suffix}`. `suffix` is appended verbatim
/// so a caller that needs a real extension at the end (e.g. `".png"`, for
/// MIME-sniffing fallback tests) can still get one. The trailing `counter`
/// (shared process-wide, like `tests/support::next_temp_path`'s) is what
/// actually guarantees uniqueness: `SystemTime::now()` alone is not — on a
/// clock whose reported resolution is coarser than a nanosecond (common on
/// macOS), two calls from different test threads can land on the same
/// instant and collide on the same path.
pub(crate) fn unique_temp_path(prefix: &str, suffix: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}-{counter}{suffix}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

struct DirectoryGuard {
    original: std::path::PathBuf,
    temporary: std::path::PathBuf,
}

impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        // Restore even when the test panics, before releasing the shared
        // lock. Otherwise one failure changes the environment of later tests.
        if std::env::set_current_dir(&self.original).is_ok() {
            let _ = std::fs::remove_dir_all(&self.temporary);
        }
    }
}

/// One process-wide lock guarding the current directory for both
/// [`in_temp_dir`] and [`in_temp_dir_async`] — a single `static` shared by
/// both functions, not one per function (a `static` declared inside each
/// function body would be a *separate* instance per function, which would
/// let a sync caller and an async caller race on the current directory
/// exactly the way this lock exists to prevent).
static CURRENT_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn enter_temp_dir(label: &str) -> DirectoryGuard {
    let dir = unique_temp_path(label, "");
    std::fs::create_dir_all(&dir).unwrap();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    DirectoryGuard {
        original,
        temporary: dir,
    }
}

/// Runs `body` with the current directory temporarily switched to a fresh,
/// empty directory named after `label`, so relative paths under it (e.g.
/// `session::SESSIONS_DIR`) resolve in isolation instead of under this
/// repository's own working tree. Serialized via one process-wide lock
/// shared by every caller regardless of module: the current directory is
/// shared mutable state, and `cargo test`'s default threaded runner would
/// otherwise let two callers race on it — not just two calls from the same
/// module, which is why this lives here instead of as a private per-module
/// helper.
pub(crate) fn in_temp_dir<T>(label: &str, body: impl FnOnce() -> T) -> T {
    let _guard = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _directory = enter_temp_dir(label);
    body()
}

/// Async counterpart to [`in_temp_dir`], for a test exercising an
/// `async_io`-backed reader/writer (`cache::load`, `cassette::load`, ...)
/// that resolves a path relative to the current directory. `body` is
/// `.await`ed while the directory swap and lock are both still held —
/// safe here because every production path this crate's async filesystem
/// helpers actually take offloads to a plain OS thread (see
/// `async_io::run_blocking`'s doc comment) rather than `tokio::spawn`, so
/// nothing requires this function's own future, or the `MutexGuard`/
/// `DirectoryGuard` it holds across the `.await`, to be `Send`. clippy can't
/// see that reasoning, only that a `std::sync::MutexGuard` crosses an
/// `.await` point — hence the `#[allow]` below rather than switching to a
/// `tokio::sync::Mutex`, which would suggest a runtime hazard that isn't
/// actually present here.
#[allow(clippy::await_holding_lock)]
pub(crate) async fn in_temp_dir_async<T>(
    label: &str,
    body: impl std::future::Future<Output = T>,
) -> T {
    let _guard = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _directory = enter_temp_dir(label);
    body.await
}
