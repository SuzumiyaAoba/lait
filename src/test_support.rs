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
