//! Advisory exclusive leases for already-open regular files.
//!
//! The workflow output path lease serializes names after canonicalization. A
//! canonical path is insufficient for hard links, though: two different names
//! can still refer to one inode. This module adds a second, descriptor-based
//! lease acquired after the output file is opened and held until the writer
//! finishes. It is intentionally advisory; external writers that do not take
//! the same lock can still race with us.

use std::{
    fs::File,
    io,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The single wording for `ExclusiveLease::acquire`'s two cancellation
/// checks (the up-front check and the one inside its poll loop).
const OUTPUT_FILE_LOCK_CANCELLED: &str = "regular output file lock was cancelled";

/// An exclusive advisory lease on an already-open regular file.
///
/// The lease is held until this value is dropped, including when the caller's
/// write returns an error. `fs2` maps this operation to `flock(2)` on Unix and
/// `LockFileEx` on Windows. Unix locks are advisory: non-cooperating external
/// writers are outside the portable guarantee of this lease.
#[derive(Debug)]
pub(crate) struct ExclusiveLease {
    file: File,
}

impl ExclusiveLease {
    /// Waits for an exclusive lease while checking the worker cancellation flag
    /// between short, bounded polls. The caller must acquire this before
    /// truncating or writing the file.
    pub(crate) fn acquire(file: &File, cancelled: &AtomicBool) -> Result<Self> {
        if cancelled.load(Ordering::Acquire) {
            bail!(crate::error::cancelled(OUTPUT_FILE_LOCK_CANCELLED));
        }
        let lock_file = file
            .try_clone()
            .context("failed to clone regular output file for locking")?;
        loop {
            if cancelled.load(Ordering::Acquire) {
                bail!(crate::error::cancelled(OUTPUT_FILE_LOCK_CANCELLED));
            }
            match lock_file.try_lock_exclusive() {
                Ok(()) => return Ok(ExclusiveLease { file: lock_file }),
                Err(error) if is_contended(&error) => {
                    thread::sleep(LOCK_POLL_INTERVAL);
                }
                Err(error) => {
                    return Err(error).context("failed to acquire regular output file lock");
                }
            }
        }
    }
}

impl Drop for ExclusiveLease {
    fn drop(&mut self) {
        // The file is about to be closed in the normal path, but explicitly
        // unlocking is required on platforms where a duplicated descriptor
        // keeps the original open-file lock alive. It also covers callers that
        // retain the descriptor after a failed write. There is no useful
        // recovery path from an unlock error during Drop, so it is ignored.
        let _ = FileExt::unlock(&self.file);
    }
}

fn is_contended(error: &io::Error) -> bool {
    match (
        error.raw_os_error(),
        fs2::lock_contended_error().raw_os_error(),
    ) {
        (Some(actual), Some(contended)) if actual == contended => true,
        _ => error.kind() == io::ErrorKind::WouldBlock,
    }
}

#[cfg(test)]
mod tests {
    use super::ExclusiveLease;
    use fs2::FileExt;
    use std::{
        fs::OpenOptions,
        io::Write,
        sync::{Arc, atomic::AtomicBool},
        thread,
        time::Duration,
    };

    fn fixture() -> (std::path::PathBuf, std::fs::File) {
        let path = crate::test_support::unique_temp_path("lait-file-lock", ".txt");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("lock fixture should open");
        file.write_all(b"original")
            .expect("lock fixture should be writable");
        (path, file)
    }

    #[test]
    fn exclusive_lease_is_released_after_successful_scope() {
        let (path, file) = fixture();
        let cancelled = AtomicBool::new(false);
        {
            let _lease = ExclusiveLease::acquire(&file, &cancelled).unwrap();
            let contender = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            assert!(FileExt::try_lock_exclusive(&contender).is_err());
        }
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::try_lock_exclusive(&contender).expect("the lease must be released after success");
        let _ = FileExt::unlock(&contender);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn exclusive_lease_is_released_when_the_write_scope_fails() {
        let (path, file) = fixture();
        let cancelled = AtomicBool::new(false);
        let failed = {
            let _lease = ExclusiveLease::acquire(&file, &cancelled).unwrap();
            let result: anyhow::Result<()> = Err(anyhow::anyhow!("synthetic write failure"));
            result
        };
        assert!(failed.is_err());
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::try_lock_exclusive(&contender).expect("the lease must be released after failure");
        let _ = FileExt::unlock(&contender);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn waiting_for_a_lease_can_be_cancelled_without_truncating_the_file() {
        let (path, file) = fixture();
        let held = ExclusiveLease::acquire(&file, &AtomicBool::new(false)).unwrap();
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::spawn(move || ExclusiveLease::acquire(&contender, &worker_cancelled));
        thread::sleep(Duration::from_millis(30));
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        let error = worker.join().unwrap().expect_err("lock wait must cancel");
        assert!(error.downcast_ref::<crate::error::Interrupted>().is_some());
        drop(held);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        let _ = std::fs::remove_file(path);
    }
}
