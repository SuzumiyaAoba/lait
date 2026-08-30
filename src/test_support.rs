//! Shared helpers for the `#[cfg(test)]` unit tests colocated in `src/`'s own
//! modules. Distinct from `tests/support`, which backs the integration tests
//! under `tests/` — those compile as a separate crate and can't reach this
//! one (or vice versa).

/// A path under the OS temp directory, unique to this process and instant:
/// `{prefix}-{pid}-{nanos}{suffix}`. `suffix` is appended verbatim so a
/// caller that needs a real extension at the end (e.g. `".png"`, for
/// MIME-sniffing fallback tests) can still get one.
pub(crate) fn unique_temp_path(prefix: &str, suffix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}{suffix}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
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
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let dir = unique_temp_path(label, "");
    std::fs::create_dir_all(&dir).unwrap();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let result = body();
    std::env::set_current_dir(original).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    result
}
