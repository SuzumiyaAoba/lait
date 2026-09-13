//! Unix FIFO-specific polling primitives [`read::read_from_file`] uses to
//! wait for a writer without an uninterruptible blocking `open`/`read`. Kept
//! separate from `read.rs` so the "a FIFO needs a writer to appear before it
//! reports EOF" concept — and the fact that it exists at all — is visible as
//! a file boundary rather than buried inside a general-purpose reader.

use std::{fs::File, io, io::Read, path::Path, time::Duration};

use anyhow::{Result, bail};

pub(super) enum FifoEvent {
    NoWriter,
    WriterConnected,
    Data(u8),
}

pub(super) fn wait_for_fifo_event(file: &mut File, path: &Path) -> Result<FifoEvent> {
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
