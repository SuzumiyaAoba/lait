//! Cancellation-aware output-file writing: [`write_output_file`], the
//! blocking worker it runs through [`super::blocking::run_blocking_with_path_lock`],
//! and the regular/special-file write paths underneath it.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

use crate::file_lock;

use super::blocking::run_blocking_with_path_lock;

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
                    std::thread::sleep(std::time::Duration::from_millis(10));
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
