//! Atomic publication of complete local snapshots (cache, cassette, checkpoint),
//! plus the shared "read a whole small YAML file, then parse it" step
//! definition files (`test`/`eval`) use to load themselves.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Directory names a recursive workflow-file discovery walk never descends
/// into, even though they don't start with `.` (dot-directories, e.g.
/// `.git`, are always skipped too by each caller's own check) — scanning
/// them would be slow, and their `.yml`/`.yaml`/`.md` files (dependency
/// manifests, changelogs, CI configs belonging to a vendored package, ...)
/// are never lait workflow/test/agent files. Shared by `lint::targets`
/// (`lait lint <DIR>`) and `test_run` (`lait test <DIR>`) — the latter used
/// to omit this list entirely, so `lait test <repo-root>` would recurse into
/// `target/`, a real cost for a Rust project's own build directory.
pub(crate) const SKIPPED_DIR_NAMES: &[&str] = &["target", "node_modules"];

/// Shared `with_context`/`.context` message shapes for plain filesystem
/// reads, listings, and directory creation — the same three actions
/// `write_atomic` below and several other modules (`cache`, `checkpoint`,
/// `lint::targets`, `test_run`) all perform on paths outside their own
/// control, repeated verbatim often enough (`config::load::config_read_error_context`
/// set the precedent) to warrant one shared spelling per action rather than
/// a `format!` at each call site.
pub(crate) fn read_context(path: &Path) -> String {
    format!("failed to read '{}'", path.display())
}
pub(crate) fn read_dir_context(path: &Path) -> String {
    format!("failed to read directory '{}'", path.display())
}
pub(crate) fn create_dir_context(path: &Path) -> String {
    format!("failed to create directory '{}'", path.display())
}

/// Reads `path` as UTF-8 (cancellable, size-bounded the same as every other
/// file this crate loads — see `async_io::MAX_READ_BYTES`) and parses it as
/// YAML into `T`. `kind` names the definition in both error messages
/// ("failed to {read,parse} {kind} definition '<path>'"), matching the
/// wording `test_run`/`eval` each used to spell out independently — down to
/// the same two `with_context` calls — for their own definition file.
pub(crate) async fn read_and_parse_yaml<T: DeserializeOwned>(
    path: &Path,
    kind: &str,
    cancellation: CancellationToken,
) -> Result<T> {
    let contents = crate::async_io::read_to_string_cancellable(
        path,
        cancellation,
        crate::async_io::MAX_READ_BYTES,
    )
    .await
    .with_context(|| format!("failed to read {kind} definition '{}'", path.display()))?;
    serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse {kind} definition '{}'", path.display()))
}

struct PendingFile(PathBuf);

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Publish with a same-directory rename. Each writer owns a distinct temporary
/// file, so concurrent writers cannot truncate or rename each other's contents.
/// This guarantees complete snapshots to readers, not crash durability (`fsync`).
pub(crate) fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| create_dir_context(parent))?;
    let name = path
        .file_name()
        .context("snapshot path must have a file name")?;
    for _ in 0..100 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = name.to_os_string();
        temporary_name.push(format!(".{}.{counter}.tmp", std::process::id()));
        let temporary_path = parent.join(temporary_name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create '{}'", temporary_path.display()));
            }
        };
        let pending = PendingFile(temporary_path);
        let written = file.write_all(body);
        // Close before propagating a write error so cleanup also works on
        // platforms that cannot unlink an open file.
        drop(file);
        written.with_context(|| format!("failed to write '{}'", pending.0.display()))?;
        fs::rename(&pending.0, path)
            .with_context(|| format!("failed to publish '{}'", path.display()))?;
        return Ok(());
    }
    bail!(
        "failed to allocate a temporary file for '{}'",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn concurrent_writers_publish_complete_snapshots_and_leave_no_temporary_files() {
        let dir = TempDir::new("lait-storage");
        let path = dir.path().join("snapshot.json");
        write_atomic(&path, &[0; 8192]).unwrap();
        std::thread::scope(|scope| {
            let barrier = std::sync::Barrier::new(8);
            let barrier = std::sync::Arc::new(barrier);
            for byte in 0..8 {
                let path = &path;
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..10 {
                        write_atomic(path, &vec![byte; 8192]).unwrap();
                        let snapshot = fs::read(path).unwrap();
                        assert_eq!(snapshot.len(), 8192);
                        assert!(snapshot.iter().all(|value| *value == snapshot[0]));
                    }
                });
            }
        });
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_publication_cleans_up_its_temporary_file() {
        let dir = TempDir::new("lait-storage");
        let path = dir.path().join("directory");
        fs::create_dir(&path).unwrap();
        assert!(write_atomic(&path, b"payload").is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
