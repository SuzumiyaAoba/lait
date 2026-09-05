//! Atomic publication of complete local snapshots (cache, cassette, checkpoint).

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory '{}'", parent.display()))?;
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

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = crate::test_support::unique_temp_path("lait-storage", "");
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn concurrent_writers_publish_complete_snapshots_and_leave_no_temporary_files() {
        let dir = TempDir::new();
        let path = dir.0.join("snapshot.json");
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
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn failed_publication_cleans_up_its_temporary_file() {
        let dir = TempDir::new();
        let path = dir.0.join("directory");
        fs::create_dir(&path).unwrap();
        assert!(write_atomic(&path, b"payload").is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }
}
