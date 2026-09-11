//! The OS process-tree containment primitive [`run_process`](super::run_process)
//! uses to make sure a killed command takes its descendants with it:
//! [`CommandProcessTree`] and its three platform variants (unix process
//! group, Windows Job Object, and a no-op fallback everywhere else). Split
//! out of `process.rs` — the platform `#[cfg]` trio is self-contained
//! plumbing behind the four methods `run_process`/`terminate_*` actually
//! call (`configure`/`attach`/`kill`/`disarm`), not part of the run loop
//! itself.

use anyhow::{Result, anyhow};

#[cfg(windows)]
use super::job_windows;
#[cfg(windows)]
use anyhow::Context;

/// Tracks the OS primitive that owns a command's process tree.
///
/// Unix commands are put in a fresh process group before they are spawned,
/// so a negative-PGID SIGKILL reaches the command and every descendant that
/// inherits the group. Windows uses a Job Object for the same ownership
/// boundary; terminating the job is the only reliable way to stop descendants
/// when the command itself is a shell or has forked workers.
#[cfg(unix)]
pub(super) struct CommandProcessTree {
    process_group: libc::pid_t,
    cleanup_on_drop: bool,
}

#[cfg(windows)]
pub(super) struct CommandProcessTree {
    job: std::os::windows::io::OwnedHandle,
    cleanup_on_drop: bool,
}

#[cfg(not(any(unix, windows)))]
pub(super) struct CommandProcessTree {
    cleanup_on_drop: bool,
}

#[cfg(unix)]
impl CommandProcessTree {
    pub(super) fn configure(command: &mut tokio::process::Command) {
        // PGID 0 asks the OS to use the child's PID as its process-group ID.
        // Tokio forwards this to std::process::Command before fork/exec, so
        // there is no parent-side race between spawning and setpgid(2).
        command.process_group(0);
    }

    pub(super) fn attach(child: &tokio::process::Child) -> Result<Self> {
        let pid = child
            .id()
            .ok_or_else(|| anyhow!("command exited before its process group was attached"))?;
        let process_group = libc::pid_t::try_from(pid)
            .map_err(|_| anyhow!("command process id {pid} does not fit in a process-group id"))?;
        Ok(Self {
            process_group,
            cleanup_on_drop: true,
        })
    }

    pub(super) fn kill(&self) -> std::io::Result<()> {
        // A negative PID targets the process group whose ID is -PID. ESRCH
        // means that the group is already empty, which is successful cleanup.
        let result = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    pub(super) fn disarm(&mut self) {
        self.cleanup_on_drop = false;
    }
}

#[cfg(unix)]
impl Drop for CommandProcessTree {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = self.kill();
        }
    }
}

#[cfg(windows)]
impl CommandProcessTree {
    pub(super) fn configure(command: &mut tokio::process::Command) {
        job_windows::configure(command);
    }

    pub(super) fn attach(child: &tokio::process::Child) -> Result<Self> {
        let process = child
            .raw_handle()
            .ok_or_else(|| anyhow!("command exited before its Windows job was attached"))?;
        let job = job_windows::attach(process)
            .context("failed to assign command process to a Windows Job Object")?;
        Ok(Self {
            job,
            cleanup_on_drop: true,
        })
    }

    pub(super) fn kill(&self) -> std::io::Result<()> {
        job_windows::terminate(std::os::windows::io::AsRawHandle::as_raw_handle(&self.job))
    }

    pub(super) fn disarm(&mut self) {
        self.cleanup_on_drop = false;
    }
}

#[cfg(windows)]
impl Drop for CommandProcessTree {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = self.kill();
        }
    }
}

#[cfg(not(any(unix, windows)))]
impl CommandProcessTree {
    pub(super) fn configure(_command: &mut tokio::process::Command) {}

    pub(super) fn attach(_child: &tokio::process::Child) -> Result<Self> {
        Ok(Self {
            cleanup_on_drop: false,
        })
    }

    pub(super) fn kill(&self) -> std::io::Result<()> {
        Ok(())
    }

    pub(super) fn disarm(&mut self) {
        self.cleanup_on_drop = false;
    }
}

#[cfg(not(any(unix, windows)))]
impl Drop for CommandProcessTree {
    fn drop(&mut self) {}
}
