//! Runs a workflow `command:` node's child process and contains its whole
//! process tree (not just the direct child) so a cancellation/timeout can't
//! leave descendants running. Extracted out of `app.rs`'s workflow
//! interpreter — the only thing this module's callers need is
//! [`run_command`]; everything else here is process-tree plumbing private to
//! that one entry point.

use anyhow::{Context, Result, anyhow, bail};

/// Tracks the OS primitive that owns a command's process tree.
///
/// Unix commands are put in a fresh process group before they are spawned,
/// so a negative-PGID SIGKILL reaches the command and every descendant that
/// inherits the group. Windows uses a Job Object for the same ownership
/// boundary; terminating the job is the only reliable way to stop descendants
/// when the command itself is a shell or has forked workers.
#[cfg(unix)]
struct CommandProcessTree {
    process_group: libc::pid_t,
    cleanup_on_drop: bool,
}

#[cfg(windows)]
struct CommandProcessTree {
    job: std::os::windows::io::OwnedHandle,
    cleanup_on_drop: bool,
}

#[cfg(not(any(unix, windows)))]
struct CommandProcessTree {
    cleanup_on_drop: bool,
}

#[cfg(unix)]
impl CommandProcessTree {
    fn configure(command: &mut tokio::process::Command) {
        // PGID 0 asks the OS to use the child's PID as its process-group ID.
        // Tokio forwards this to std::process::Command before fork/exec, so
        // there is no parent-side race between spawning and setpgid(2).
        command.process_group(0);
    }

    fn attach(child: &tokio::process::Child) -> Result<Self> {
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

    fn kill(&self) -> std::io::Result<()> {
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

    fn disarm(&mut self) {
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
mod windows_command_job {
    use std::{
        ffi::c_void,
        io,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
        ptr,
    };

    type Bool = i32;

    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: u32 = 0x0000_0002;
    const INVALID_HANDLE_VALUE: RawHandle = -1isize as RawHandle;
    const INVALID_RESUME_COUNT: u32 = u32::MAX;

    #[repr(C)]
    struct ThreadEntry32 {
        size: u32,
        usage: u32,
        thread_id: u32,
        owner_process_id: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "CreateJobObjectW"]
        fn create_job_object_w(lp_job_attributes: *mut c_void, lp_name: *const u16) -> RawHandle;
        #[link_name = "AssignProcessToJobObject"]
        fn assign_process_to_job_object(job: RawHandle, process: RawHandle) -> Bool;
        #[link_name = "TerminateJobObject"]
        fn terminate_job_object(job: RawHandle, exit_code: u32) -> Bool;
        #[link_name = "CreateToolhelp32Snapshot"]
        fn create_toolhelp32_snapshot(flags: u32, process_id: u32) -> RawHandle;
        #[link_name = "Thread32First"]
        fn thread32_first(snapshot: RawHandle, entry: *mut ThreadEntry32) -> Bool;
        #[link_name = "Thread32Next"]
        fn thread32_next(snapshot: RawHandle, entry: *mut ThreadEntry32) -> Bool;
        #[link_name = "OpenThread"]
        fn open_thread(access: u32, inherit_handle: Bool, thread_id: u32) -> RawHandle;
        #[link_name = "ResumeThread"]
        fn resume_thread(thread: RawHandle) -> u32;
        #[link_name = "CloseHandle"]
        fn close_handle(handle: RawHandle) -> Bool;
        #[link_name = "GetProcessId"]
        fn get_process_id(process: RawHandle) -> u32;
    }

    pub(super) fn configure(command: &mut tokio::process::Command) {
        // Keep the primary thread stopped until the process has been assigned
        // to our Job Object.  Without this, a shell can create descendants in
        // the interval between CreateProcess and AssignProcessToJobObject;
        // those descendants would not inherit the job and would survive a
        // later timeout.
        command.creation_flags(CREATE_SUSPENDED);
    }

    fn resume_process(process: RawHandle) -> io::Result<()> {
        let process_id = unsafe { get_process_id(process) };
        if process_id == 0 {
            return Err(io::Error::last_os_error());
        }

        let snapshot = unsafe { create_toolhelp32_snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let result = (|| {
            let mut entry = ThreadEntry32 {
                size: std::mem::size_of::<ThreadEntry32>() as u32,
                usage: 0,
                thread_id: 0,
                owner_process_id: 0,
                base_priority: 0,
                delta_priority: 0,
                flags: 0,
            };
            let mut found_thread = false;
            let mut has_entry = unsafe { thread32_first(snapshot, &mut entry) } != 0;
            while has_entry {
                if entry.owner_process_id == process_id {
                    found_thread = true;
                    let thread = unsafe { open_thread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
                    if thread.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let resume_result = unsafe { resume_thread(thread) };
                    let close_result = unsafe { close_handle(thread) };
                    if resume_result == INVALID_RESUME_COUNT {
                        return Err(io::Error::last_os_error());
                    }
                    if close_result == 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                has_entry = unsafe { thread32_next(snapshot, &mut entry) } != 0;
            }

            if !found_thread {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "suspended command thread was not found",
                ));
            }
            Ok(())
        })();
        let close_result = unsafe { close_handle(snapshot) };
        if result.is_ok() && close_result == 0 {
            return Err(io::Error::last_os_error());
        }
        result
    }

    pub(super) fn attach(process: RawHandle) -> io::Result<OwnedHandle> {
        // A private, unnamed job has no ambient permissions or namespace
        // concerns. The handle remains owned by CommandProcessTree until the
        // command completes or cancellation cleanup runs.
        let job = unsafe { create_job_object_w(ptr::null_mut(), ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a newly-owned kernel handle.
        let job = unsafe { OwnedHandle::from_raw_handle(job) };
        let result = unsafe { assign_process_to_job_object(job.as_raw_handle(), process) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        if let Err(error) = resume_process(process) {
            // The process is still suspended if resuming failed.  Terminate
            // the Job before returning so a partially resumed process (or any
            // descendant it managed to create) cannot escape this failed
            // attach path.  The caller also kills/reaps the direct child.
            let _ = unsafe { terminate_job_object(job.as_raw_handle(), 1) };
            return Err(error);
        }
        Ok(job)
    }

    pub(super) fn terminate(job: RawHandle) -> io::Result<()> {
        let result = unsafe { terminate_job_object(job, 1) };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl CommandProcessTree {
    fn configure(command: &mut tokio::process::Command) {
        windows_command_job::configure(command);
    }

    fn attach(child: &tokio::process::Child) -> Result<Self> {
        let process = child
            .raw_handle()
            .ok_or_else(|| anyhow!("command exited before its Windows job was attached"))?;
        let job = windows_command_job::attach(process)
            .context("failed to assign command process to a Windows Job Object")?;
        Ok(Self {
            job,
            cleanup_on_drop: true,
        })
    }

    fn kill(&self) -> std::io::Result<()> {
        windows_command_job::terminate(std::os::windows::io::AsRawHandle::as_raw_handle(&self.job))
    }

    fn disarm(&mut self) {
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
    fn configure(_command: &mut tokio::process::Command) {}

    fn attach(_child: &tokio::process::Child) -> Result<Self> {
        Ok(Self {
            cleanup_on_drop: false,
        })
    }

    fn kill(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn disarm(&mut self) {
        self.cleanup_on_drop = false;
    }
}

#[cfg(not(any(unix, windows)))]
impl Drop for CommandProcessTree {
    fn drop(&mut self) {}
}

/// Terminates a command's process tree and reaps its direct child. The
/// containment primitive is intentionally attempted twice: a child can fork
/// a descendant in the small interval between the first termination request
/// and the direct child's exit, and that descendant inherits the same group or
/// Job Object.
async fn terminate_command_process_tree(
    process_tree: &CommandProcessTree,
    child: &mut tokio::process::Child,
) -> Result<()> {
    let tree_kill_error = process_tree.kill().err().map(|error| error.to_string());
    let direct_kill_error = if tree_kill_error.is_some() {
        // Keep a direct-child fallback for an unavailable or rejected OS
        // containment primitive. The caller still receives the containment
        // error, so it cannot mistake this fallback for tree cleanup.
        child.kill().await.err().map(|error| error.to_string())
    } else {
        None
    };
    let reap_error = child.wait().await.err().map(|error| error.to_string());
    let second_tree_kill_error = process_tree.kill().err().map(|error| error.to_string());

    if let Some(error) = &tree_kill_error {
        let mut message = format!("failed to terminate command process tree: {error}");
        if let Some(direct_error) = &direct_kill_error {
            message.push_str(&format!(
                "; failed to kill the direct child: {direct_error}"
            ));
        }
        if let Some(reap_error) = &reap_error {
            message.push_str(&format!("; failed to reap it: {reap_error}"));
        }
        bail!(message);
    }
    if let Some(error) = &second_tree_kill_error {
        let mut message =
            format!("failed to terminate command process tree after reaping: {error}");
        if let Some(reap_error) = &reap_error {
            message.push_str(&format!("; failed to reap it: {reap_error}"));
        }
        bail!(message);
    }
    if let Some(error) = &reap_error {
        bail!("failed to reap command: {error}");
    }
    Ok(())
}

/// Runs `argv[0]` as a child process with `argv[1..]` as its arguments,
/// piping `stdin_input` to its stdin and capturing stdout as this node's
/// output (see `crate::workflow::NodeDefinition::command`'s doc comment for
/// the full contract). `stdin`/stdout/stderr are handled by separate tasks
/// while the child is awaited so a command that writes a lot of output
/// before reading all of stdin (or never reads stdin at all) can't deadlock
/// against this side's pipes filling up.
pub(crate) async fn run_command(
    argv: &[String],
    stdin_input: &str,
    mut step_cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (program, args) = argv
        .split_first()
        .expect("validate::validate_node rejects an empty 'command' list");

    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // The enclosing node timeout sends a cancellation signal that
        // performs kill + wait, while this also protects the child if this
        // future is cancelled by a caller.
        .kill_on_drop(true);
    CommandProcessTree::configure(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run command '{program}'"))?;
    let mut process_tree = match CommandProcessTree::attach(&child) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            // On Windows, a process can already be inside an incompatible
            // outer Job Object. Do not leave the just-spawned child running if
            // attaching our containment boundary fails.
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error).with_context(|| format!("failed to contain command '{program}'"));
        }
    };

    let mut stdin = child.stdin.take().expect("stdin was requested as piped");
    let stdin_input = stdin_input.to_owned();
    let write_stdin = tokio::spawn(async move {
        stdin.write_all(stdin_input.as_bytes()).await?;
        stdin.shutdown().await
    });

    let mut stdout = child.stdout.take().expect("stdout was requested as piped");
    let mut read_stdout = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let mut stderr = child.stderr.take().expect("stderr was requested as piped");
    let mut read_stderr = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });

    let status = if let Some(step_cancel) = &mut step_cancel {
        loop {
            tokio::select! {
                status = child.wait() => {
                    break status.with_context(|| format!("failed to run command '{program}'"))?;
                }
                changed = step_cancel.changed() => {
                    if changed.is_ok() && !*step_cancel.borrow() {
                        continue;
                    }

                    // Closing the parent's stdin first prevents a writer
                    // blocked behind a descendant-inherited pipe from
                    // delaying command cleanup. Terminate the process group /
                    // Job Object so the retry cannot overlap any descendant.
                    write_stdin.abort();
                    let _ = write_stdin.await;
                    let cleanup_result =
                        terminate_command_process_tree(&process_tree, &mut child).await;

                    // A descendant may retain stdout/stderr after the direct
                    // child exits. Those readers are no longer needed once
                    // the command is cancelled, so abort them rather than
                    // waiting for an unrelated descendant to close its copy.
                    read_stdout.abort();
                    read_stderr.abort();
                    let _ = read_stdout.await;
                    let _ = read_stderr.await;
                    cleanup_result.with_context(|| format!("command '{program}' was cancelled"))?;
                    bail!("command '{program}' was cancelled");
                }
            }
        }
    } else {
        child
            .wait()
            .await
            .with_context(|| format!("failed to run command '{program}'"))?
    };

    // Once the direct child has exited, no more input can affect this
    // command. Abort the writer instead of awaiting it: a descendant that
    // inherited the child's stdin can otherwise keep the pipe open forever.
    write_stdin.abort();
    let _ = write_stdin.await;
    let (stdout, stderr) = if let Some(step_cancel) = &mut step_cancel {
        let mut stdout_bytes = None;
        let mut stderr_bytes = None;
        loop {
            tokio::select! {
                result = &mut read_stdout, if stdout_bytes.is_none() => {
                    stdout_bytes = Some(
                        result
                            .context("command stdout read task panicked")?
                            .with_context(|| format!("failed to read stdout from command '{program}'"))?,
                    );
                }
                result = &mut read_stderr, if stderr_bytes.is_none() => {
                    stderr_bytes = Some(
                        result
                            .context("command stderr read task panicked")?
                            .with_context(|| format!("failed to read stderr from command '{program}'"))?,
                    );
                }
                changed = step_cancel.changed() => {
                    if changed.is_ok() && !*step_cancel.borrow() {
                        continue;
                    }
                    let tree_kill_result = process_tree.kill();
                    read_stdout.abort();
                    read_stderr.abort();
                    let _ = read_stdout.await;
                    let _ = read_stderr.await;
                    if let Err(error) = tree_kill_result {
                        bail!(
                            "command '{program}' was cancelled; failed to terminate its process tree: {error}"
                        );
                    }
                    bail!("command '{program}' was cancelled");
                }
            }
            if let Some(stdout) = stdout_bytes.take() {
                if let Some(stderr) = stderr_bytes.take() {
                    break (stdout, stderr);
                }
                stdout_bytes = Some(stdout);
            }
        }
    } else {
        let stdout = read_stdout
            .await
            .context("command stdout read task panicked")?
            .with_context(|| format!("failed to read stdout from command '{program}'"))?;
        let stderr = read_stderr
            .await
            .context("command stderr read task panicked")?
            .with_context(|| format!("failed to read stderr from command '{program}'"))?;
        (stdout, stderr)
    };

    // No cancellation path remains after both readers have completed. Keep
    // intentionally backgrounded processes from being killed on a successful
    // command return; cancellation and future-drop paths remain armed until
    // they have explicitly terminated the process tree.
    process_tree.disarm();

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        bail!(
            "command '{program}' exited with {}: {}",
            status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(stdout).map_err(|_| {
        anyhow!("command '{program}' produced non-UTF-8 output on stdout; binary output is not supported")
    })?;
    Ok(strip_one_trailing_line_ending(stdout))
}

/// Removes one line ending from command output. A CRLF pair counts as one
/// line ending, while additional trailing line endings remain part of the
/// command's output.
fn strip_one_trailing_line_ending(mut output: String) -> String {
    if output.ends_with("\r\n") {
        output.truncate(output.len() - 2);
    } else if output.ends_with(['\n', '\r']) {
        output.truncate(output.len() - 1);
    }
    output
}
