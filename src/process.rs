//! Runs a workflow `command:` node's child process and contains its whole
//! process tree (not just the direct child) so a cancellation/timeout can't
//! leave descendants running. Extracted out of `app.rs`'s workflow
//! interpreter — the only thing this module's callers need is
//! [`run_command`]; everything else here is process-tree plumbing private to
//! that one entry point.

use std::{process::ExitStatus, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Maximum bytes retained from either stdout or stderr of one command.
/// Keeping this aligned with file-backed input limits prevents a configured
/// command (or a model-invoked shell tool) from turning an unbounded pipe into
/// an unbounded allocation.
pub(crate) const MAX_COMMAND_OUTPUT_BYTES: usize = crate::async_io::MAX_READ_BYTES;

/// A failed containment primitive must not make cancellation wait forever for
/// a child that could not be killed. Normal SIGKILL/Job termination reaps well
/// within this bound; an elapsed bound is surfaced as a cleanup error.
const COMMAND_REAP_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(windows)]
mod job_windows;
mod tree;

use tree::CommandProcessTree;

/// Terminates a command's process tree and reaps its direct child. The
/// containment primitive is intentionally attempted twice: a child can fork
/// a descendant in the small interval between the first termination request
/// and the direct child's exit, and that descendant inherits the same group or
/// Job Object.
async fn terminate_command_process_tree(
    process_tree: &CommandProcessTree,
    child: &mut tokio::process::Child,
) -> Result<()> {
    let tree_kill_error = process_tree.kill().err();
    let direct_kill_error = if tree_kill_error.is_some() {
        // Keep a direct-child fallback for an unavailable or rejected OS
        // containment primitive. A second group/job probe below decides
        // whether this fallback actually left the owned boundary empty.
        child.start_kill().err()
    } else {
        None
    };
    let mut reap_error = match tokio::time::timeout(COMMAND_REAP_TIMEOUT, child.wait()).await {
        Ok(result) => result.err(),
        Err(_) => Some(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "timed out after {} second(s)",
                COMMAND_REAP_TIMEOUT.as_secs()
            ),
        )),
    };
    let second_tree_kill_error = process_tree.kill().err();

    // A sandbox can reject the first group/job termination even though the
    // direct-child fallback succeeds. If direct-child reaping succeeded and
    // the second containment request also succeeded (whether it killed
    // remaining members or found none), the owned boundary is cleaned up, so
    // the initial error is not a cleanup failure to propagate.
    if (reap_error.is_some() || second_tree_kill_error.is_some())
        && let Some(error) = tree_kill_error
    {
        let mut cleanup_error = anyhow::Error::new(error);
        if let Some(direct_error) = direct_kill_error {
            cleanup_error =
                cleanup_error.context(format!("failed to kill the direct child: {direct_error}"));
        }
        if let Some(reap_error) = reap_error.take() {
            cleanup_error = cleanup_error.context(format!("failed to reap it: {reap_error}"));
        }
        return Err(cleanup_error.context("failed to terminate command process tree"));
    }
    if let Some(error) = second_tree_kill_error {
        let mut cleanup_error = anyhow::Error::new(error);
        if let Some(reap_error) = reap_error.take() {
            cleanup_error = cleanup_error.context(format!("failed to reap it: {reap_error}"));
        }
        return Err(cleanup_error.context("failed to terminate command process tree after reaping"));
    }
    if let Some(error) = reap_error {
        return Err(anyhow::Error::new(error).context("failed to reap command"));
    }
    Ok(())
}

/// Captures one command stream while enforcing the same 16 MiB budget used by
/// file-backed inputs. Reading in bounded chunks lets the caller observe an
/// oversized stream as soon as the first chunk crosses the limit.
async fn read_limited<R>(
    mut reader: R,
    command_kind: &'static str,
    stream_name: &'static str,
    max_output_bytes: usize,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; CHUNK_SIZE];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .with_context(|| format!("failed to read {command_kind} {stream_name}"))?;
        if read == 0 {
            return Ok(bytes);
        }
        let Some(next_len) = bytes.len().checked_add(read) else {
            bail!("{command_kind} {stream_name} output size overflowed");
        };
        if next_len > max_output_bytes {
            bail!(
                "{command_kind} {stream_name} output exceeds the configured limit of {max_output_bytes} bytes"
            );
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

fn spawn_output_reader<R>(
    reader: R,
    command_kind: &'static str,
    stream_name: &'static str,
    max_output_bytes: usize,
) -> JoinHandle<Result<Vec<u8>>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(read_limited(
        reader,
        command_kind,
        stream_name,
        max_output_bytes,
    ))
}

/// Aborts spawned Tokio tasks if their owning command future is dropped. A
/// `JoinHandle` normally detaches its task on drop; retaining abort handles
/// keeps a cancelled or abandoned command from leaving pipe readers alive.
struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl AbortOnDrop {
    fn new(handles: impl IntoIterator<Item = tokio::task::AbortHandle>) -> Self {
        Self(handles.into_iter().collect())
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

async fn abort_optional_task<T>(task: &mut Option<JoinHandle<T>>) {
    if let Some(task) = task.take() {
        task.abort();
        let _ = task.await;
    }
}

/// Stops all command-owned async tasks after cancellation or a reader error.
/// Readers are stored as options so a completed `JoinHandle` is consumed once
/// and can never be polled a second time during a later cleanup path.
async fn abort_process_tasks(
    write_stdin: &mut Option<JoinHandle<Result<()>>>,
    read_stdout: &mut Option<JoinHandle<Result<Vec<u8>>>>,
    read_stderr: &mut Option<JoinHandle<Result<Vec<u8>>>>,
) {
    abort_optional_task(write_stdin).await;
    abort_optional_task(read_stdout).await;
    abort_optional_task(read_stderr).await;
}

/// Contains the process tree once a run is ending for any reason (a reader
/// task failed, the run was cancelled, or its timeout elapsed). A completed
/// child has already been reaped by `child.wait`, so only its descendants
/// need the process-tree termination call in that case — and not even that
/// if an end-of-loop `kill_descendants_after_exit` cleanup already ran it.
/// Shared by `reader_failed` and both the cancellation and timeout `select!`
/// arms in `run_process`, which each computed this same decision inline
/// before converging on it here.
async fn contain_process_tree(
    process_tree: &CommandProcessTree,
    child: &mut tokio::process::Child,
    child_reaped: bool,
    descendants_terminated: bool,
) -> Result<()> {
    if child_reaped {
        if !descendants_terminated {
            terminate_reaped_process_tree(process_tree).await?;
        }
        Ok(())
    } else {
        terminate_command_process_tree(process_tree, child).await
    }
}

/// The three background tasks one `run_process` invocation owns (the stdin
/// writer, the stdout/stderr readers), bundled into one parameter so a
/// cleanup helper taking them alongside its other arguments doesn't trip
/// `clippy::too_many_arguments`.
struct ProcessTasks<'a> {
    write_stdin: &'a mut Option<JoinHandle<Result<()>>>,
    read_stdout: &'a mut Option<JoinHandle<Result<Vec<u8>>>>,
    read_stderr: &'a mut Option<JoinHandle<Result<Vec<u8>>>>,
}

/// Finishes handling one stdout/stderr reader task that failed: aborts every
/// other in-flight task, contains the process tree, and builds the error
/// `run_process` should return. Its two `select!` arms are otherwise
/// byte-for-byte identical from here on — only which `Option` gets cleared
/// and which `outcome` field gets set on the *success* side differs, and
/// that (plus the `drop(child_wait)` immediately before this call, required
/// so the pinned `child.wait()` future's borrow of `child` ends before this
/// can take `&mut child`) can't be pulled into a shared `select!` future, so
/// this factors out everything that can be: the whole recovery path.
async fn reader_failed(
    error: anyhow::Error,
    process_tree: &CommandProcessTree,
    child: &mut tokio::process::Child,
    child_reaped: bool,
    descendants_terminated: bool,
    command_kind: &'static str,
    tasks: ProcessTasks<'_>,
) -> anyhow::Error {
    abort_process_tasks(tasks.write_stdin, tasks.read_stdout, tasks.read_stderr).await;
    if let Err(cleanup_error) =
        contain_process_tree(process_tree, child, child_reaped, descendants_terminated).await
    {
        return error.context(format!(
            "failed to terminate {command_kind} process tree after output reader failure: {cleanup_error:#}"
        ));
    }
    error
}

/// Terminates descendants after the direct child has already been reaped.
/// Retry once after yielding: on macOS a process group can briefly transition
/// between the reader closing its pipe and the kernel reaping the last member.
/// An error on both attempts is retained, so a denied containment operation is
/// never silently treated as successful cleanup.
async fn terminate_reaped_process_tree(process_tree: &CommandProcessTree) -> Result<()> {
    if let Err(first_error) = process_tree.kill() {
        tokio::task::yield_now().await;
        if let Err(second_error) = process_tree.kill() {
            bail!(
                "failed to terminate command descendants: {first_error}; retry failed: {second_error}"
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StdinMode<'a> {
    Pipe(&'a str),
    Null,
}

struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn join_reader_result(
    result: std::result::Result<Result<Vec<u8>>, tokio::task::JoinError>,
    command_kind: &'static str,
    stream_name: &'static str,
) -> Result<Vec<u8>> {
    result
        .with_context(|| format!("{command_kind} {stream_name} reader task panicked"))?
        .with_context(|| format!("failed to read {stream_name} from {command_kind}"))
}

async fn join_reader_task(
    reader: &mut Option<JoinHandle<Result<Vec<u8>>>>,
    command_kind: &'static str,
    stream_name: &'static str,
) -> Result<Vec<u8>> {
    let Some(reader) = reader.as_mut() else {
        bail!("{command_kind} {stream_name} reader task was unavailable");
    };
    join_reader_result(reader.await, command_kind, stream_name)
}

struct ProcessRun<'a> {
    argv: &'a [String],
    stdin: StdinMode<'a>,
    timeout: Option<Duration>,
    max_output_bytes: usize,
    cancellation: Option<CancellationToken>,
    command_kind: &'static str,
    kill_descendants_after_exit: bool,
}

/// The four pieces of terminal state `run_process`'s `tokio::select!` loop
/// accumulates as they each become available (child exit status, stdout,
/// stderr) or changes once (`descendants_terminated`), out of the ~13 local
/// bindings that function owns overall — bundled together (rather than left
/// as four separate `let mut` bindings) purely to give the loop's "have we
/// collected everything yet?" check and the post-loop conversion into
/// [`CapturedOutput`] a named home; the `tokio::select!` loop itself is not
/// restructured (see the design plan's C5 note on why splitting it further
/// risks a race).
#[derive(Default)]
struct ProcessOutcome {
    child_status: Option<Result<ExitStatus>>,
    stdout_bytes: Option<Vec<u8>>,
    stderr_bytes: Option<Vec<u8>>,
    descendants_terminated: bool,
}

impl ProcessOutcome {
    fn is_complete(&self) -> bool {
        self.child_status.is_some() && self.stdout_bytes.is_some() && self.stderr_bytes.is_some()
    }

    /// Converts the loop's terminal state into the process's captured
    /// output. The `ok_or_else` arms should be unreachable in practice —
    /// the loop above only exits once [`Self::is_complete`] holds — but stay
    /// as errors rather than `unwrap`/`expect` since they cross an `async`
    /// cancellation boundary this function does not fully control.
    fn into_captured_output(self, command_kind: &str) -> Result<CapturedOutput> {
        let status = self
            .child_status
            .and_then(Result::ok)
            .ok_or_else(|| anyhow!("{command_kind} completed without an exit status"))?;
        let stdout = self
            .stdout_bytes
            .ok_or_else(|| anyhow!("{command_kind} completed without stdout"))?;
        let stderr = self
            .stderr_bytes
            .ok_or_else(|| anyhow!("{command_kind} completed without stderr"))?;
        Ok(CapturedOutput {
            status,
            stdout,
            stderr,
        })
    }
}

async fn run_process(request: ProcessRun<'_>) -> Result<CapturedOutput> {
    let Some((program, args)) = request.argv.split_first() else {
        bail!(
            "{} must include at least one argv element",
            request.command_kind
        );
    };
    let command_kind = request.command_kind;
    if request
        .cancellation
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        bail!(crate::error::Interrupted::cancelled(format!(
            "{} '{program}' was cancelled",
            request.command_kind
        )));
    }

    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(match request.stdin {
            StdinMode::Pipe(_) => std::process::Stdio::piped(),
            StdinMode::Null => std::process::Stdio::null(),
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    CommandProcessTree::configure(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run {command_kind} '{program}'"))?;
    let mut process_tree = match CommandProcessTree::attach(&child) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(COMMAND_REAP_TIMEOUT, child.wait()).await;
            return Err(error)
                .with_context(|| format!("failed to contain {command_kind} '{program}'"));
        }
    };

    let mut write_stdin = match request.stdin {
        StdinMode::Pipe(stdin_input) => {
            let Some(mut stdin) = child.stdin.take() else {
                let error = anyhow!("{command_kind} stdin pipe was unavailable");
                let _ = terminate_command_process_tree(&process_tree, &mut child).await;
                return Err(error);
            };
            let stdin_input = stdin_input.to_owned();
            Some(tokio::spawn(async move {
                stdin.write_all(stdin_input.as_bytes()).await?;
                stdin.shutdown().await?;
                Ok::<(), anyhow::Error>(())
            }))
        }
        StdinMode::Null => None,
    };

    let Some(stdout) = child.stdout.take() else {
        let error = anyhow!("{command_kind} stdout pipe was unavailable");
        abort_optional_task(&mut write_stdin).await;
        let _ = terminate_command_process_tree(&process_tree, &mut child).await;
        return Err(error);
    };
    let Some(stderr) = child.stderr.take() else {
        let error = anyhow!("{command_kind} stderr pipe was unavailable");
        abort_optional_task(&mut write_stdin).await;
        let _ = terminate_command_process_tree(&process_tree, &mut child).await;
        return Err(error);
    };

    let mut read_stdout = Some(spawn_output_reader(
        stdout,
        request.command_kind,
        "stdout",
        request.max_output_bytes,
    ));
    let mut read_stderr = Some(spawn_output_reader(
        stderr,
        request.command_kind,
        "stderr",
        request.max_output_bytes,
    ));
    let mut task_handles = Vec::with_capacity(3);
    if let Some(write_stdin) = &write_stdin {
        task_handles.push(write_stdin.abort_handle());
    }
    if let Some(read_stdout) = &read_stdout {
        task_handles.push(read_stdout.abort_handle());
    }
    if let Some(read_stderr) = &read_stderr {
        task_handles.push(read_stderr.abort_handle());
    }
    let _task_guard = AbortOnDrop::new(task_handles);

    let mut child_wait = Box::pin(child.wait());
    let deadline = request.timeout.map(tokio::time::sleep);
    tokio::pin!(deadline);
    let mut outcome = ProcessOutcome::default();

    loop {
        if outcome.is_complete() {
            break;
        }

        tokio::select! {
            biased;
            () = async {
                match request.cancellation.as_ref() {
                    Some(cancellation) => cancellation.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                drop(child_wait);
                let cleanup = contain_process_tree(
                    &process_tree,
                    &mut child,
                    outcome.child_status.as_ref().is_some_and(Result::is_ok),
                    outcome.descendants_terminated,
                )
                .await;
                abort_process_tasks(&mut write_stdin, &mut read_stdout, &mut read_stderr).await;
                let message = format!("{} '{program}' was cancelled", request.command_kind);
                return match cleanup {
                    Ok(()) => Err(anyhow::Error::new(crate::error::Interrupted::cancelled(message))),
                    Err(error) => Err(error.context(crate::error::Interrupted::cancelled(
                        format!(
                            "{message}; failed to terminate its process tree"
                        ),
                    ))),
                };
            }
            status = &mut child_wait, if outcome.child_status.is_none() => {
                outcome.child_status = Some(
                    status.with_context(|| format!("failed to wait for {command_kind} '{program}'")),
                );
            }
            result = join_reader_task(&mut read_stdout, request.command_kind, "stdout"), if read_stdout.is_some() => {
                read_stdout = None;
                match result {
                    Ok(bytes) => outcome.stdout_bytes = Some(bytes),
                    Err(error) => {
                        let child_reaped = outcome.child_status.as_ref().is_some_and(Result::is_ok);
                        drop(child_wait);
                        return Err(reader_failed(
                            error,
                            &process_tree,
                            &mut child,
                            child_reaped,
                            outcome.descendants_terminated,
                            command_kind,
                            ProcessTasks {
                                write_stdin: &mut write_stdin,
                                read_stdout: &mut read_stdout,
                                read_stderr: &mut read_stderr,
                            },
                        ).await);
                    }
                }
            }
            result = join_reader_task(&mut read_stderr, request.command_kind, "stderr"), if read_stderr.is_some() => {
                read_stderr = None;
                match result {
                    Ok(bytes) => outcome.stderr_bytes = Some(bytes),
                    Err(error) => {
                        let child_reaped = outcome.child_status.as_ref().is_some_and(Result::is_ok);
                        drop(child_wait);
                        return Err(reader_failed(
                            error,
                            &process_tree,
                            &mut child,
                            child_reaped,
                            outcome.descendants_terminated,
                            command_kind,
                            ProcessTasks {
                                write_stdin: &mut write_stdin,
                                read_stdout: &mut read_stdout,
                                read_stderr: &mut read_stderr,
                            },
                        ).await);
                    }
                }
            }
            () = async {
                match deadline.as_mut().as_pin_mut() {
                    Some(deadline) => deadline.await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let Some(timeout) = request.timeout else {
                    return Err(anyhow!("command deadline branch was enabled without a timeout"));
                };
                let message = format!(
                    "{command_kind} '{program}' timed out after {} seconds",
                    timeout.as_secs_f64(),
                );
                drop(child_wait);
                let cleanup = contain_process_tree(
                    &process_tree,
                    &mut child,
                    outcome.child_status.as_ref().is_some_and(Result::is_ok),
                    outcome.descendants_terminated,
                )
                .await;
                abort_process_tasks(&mut write_stdin, &mut read_stdout, &mut read_stderr).await;
                if let Err(cleanup_error) = cleanup {
                    return Err(cleanup_error.context(crate::error::Interrupted::timed_out(
                        format!("{message}; failed to terminate its process tree"),
                    )));
                }
                return Err(anyhow::Error::new(crate::error::Interrupted::timed_out(message)));
            }
        }

        if outcome.child_status.as_ref().is_some_and(Result::is_err) {
            let error = outcome
                .child_status
                .take()
                .and_then(Result::err)
                .unwrap_or_else(|| anyhow!("command wait failed without an error"));
            drop(child_wait);
            abort_process_tasks(&mut write_stdin, &mut read_stdout, &mut read_stderr).await;
            if let Err(cleanup_error) =
                terminate_command_process_tree(&process_tree, &mut child).await
            {
                return Err(error.context(format!(
                    "failed to terminate {command_kind} process tree after wait failure: {cleanup_error:#}"
                )));
            }
            return Err(error);
        }

        if request.kill_descendants_after_exit
            && !outcome.descendants_terminated
            && outcome.child_status.as_ref().is_some_and(Result::is_ok)
        {
            if let Err(error) = terminate_reaped_process_tree(&process_tree).await {
                abort_process_tasks(&mut write_stdin, &mut read_stdout, &mut read_stderr).await;
                return Err(anyhow!(
                    "failed to clean up {command_kind} process tree: {error}"
                ));
            }
            outcome.descendants_terminated = true;
        }
    }

    drop(child_wait);
    abort_optional_task(&mut write_stdin).await;
    process_tree.disarm();
    outcome.into_captured_output(command_kind)
}

/// Runs a workflow/shell-tool command through the shared process runner.
pub(crate) async fn run_command(
    argv: &[String],
    stdin_input: &str,
    step_cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let output = run_process(ProcessRun {
        argv,
        stdin: StdinMode::Pipe(stdin_input),
        timeout: None,
        max_output_bytes: MAX_COMMAND_OUTPUT_BYTES,
        cancellation: step_cancel,
        command_kind: "command",
        kill_descendants_after_exit: false,
    })
    .await?;
    let program = argv.first().map(String::as_str).unwrap_or("<empty>");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "command '{program}' exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        anyhow!("command '{program}' produced non-UTF-8 output on stdout; binary output is not supported")
    })?;
    Ok(strip_one_trailing_line_ending(stdout))
}

/// The bounded output of a command whose stdout is consumed by another
/// subsystem (currently `api_key_cmd`). Stderr is drained by the shared
/// runner but deliberately not returned.
#[derive(Debug)]
pub(crate) struct BoundedCommandOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
}

/// Runs a secret-manager command through the shared runner. Its null stdin,
/// deadline, and process-tree policy ensure that inherited descriptors cannot
/// make a finite command wait forever.
pub(crate) async fn run_bounded_command(
    argv: &[String],
    timeout: Duration,
    max_output_bytes: usize,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<BoundedCommandOutput> {
    let output = run_process(ProcessRun {
        argv,
        stdin: StdinMode::Null,
        timeout: Some(timeout),
        max_output_bytes,
        cancellation,
        command_kind: "secret command",
        kill_descendants_after_exit: true,
    })
    .await?;
    Ok(BoundedCommandOutput {
        status: output.status,
        stdout: output.stdout,
    })
}

/// Removes one line ending from command output. A CRLF pair counts as one
/// line ending, while additional trailing line endings remain part of the
/// command's output.
pub(crate) fn strip_one_trailing_line_ending(mut output: String) -> String {
    if output.ends_with("\r\n") {
        output.truncate(output.len() - 2);
    } else if output.ends_with(['\n', '\r']) {
        output.truncate(output.len() - 1);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::run_command;
    #[cfg(unix)]
    use super::{MAX_COMMAND_OUTPUT_BYTES, run_bounded_command};
    #[cfg(unix)]
    use std::time::Duration;

    #[tokio::test]
    async fn rejects_an_empty_argv_without_panicking() {
        let error = run_command(&[], "", None)
            .await
            .expect_err("an empty command must be rejected");

        assert!(error.to_string().contains("at least one argv"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kills_the_process_tree_when_stdout_exceeds_the_capture_limit() {
        let argv = ["sh".to_owned(), "-c".to_owned(), "yes".to_owned()];
        let error = tokio::time::timeout(Duration::from_secs(3), run_command(&argv, "", None))
            .await
            .expect("an oversized stdout stream must be stopped promptly")
            .expect_err("an oversized stdout stream must fail");

        let details = format!("{error:#}");
        assert!(
            details.contains(&MAX_COMMAND_OUTPUT_BYTES.to_string()),
            "{details}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kills_the_process_tree_when_stderr_exceeds_the_capture_limit() {
        let argv = ["sh".to_owned(), "-c".to_owned(), "yes >&2".to_owned()];
        let error = tokio::time::timeout(Duration::from_secs(3), run_command(&argv, "", None))
            .await
            .expect("an oversized stderr stream must be stopped promptly")
            .expect_err("an oversized stderr stream must fail");

        let details = format!("{error:#}");
        assert!(
            details.contains(&MAX_COMMAND_OUTPUT_BYTES.to_string()),
            "{details}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_limit_cleanup_stops_descendants_that_hold_a_pipe_open() {
        let marker = crate::test_support::unique_temp_path("lait-process-descendant", ".marker");
        let script = format!("(sleep 1; touch '{}') & yes", marker.display());
        let argv = ["sh".to_owned(), "-c".to_owned(), script];
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            run_bounded_command(&argv, Duration::from_secs(3), 4096, None),
        )
        .await
        .expect("an oversized stream must be stopped promptly")
        .expect_err("an oversized stream must fail");

        let details = format!("{error:#}");
        assert!(details.contains("output exceeds"), "{details}");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "a descendant holding the pipe survived output-limit cleanup"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_runner_does_not_wait_for_a_successful_childs_pipe_holding_descendant() {
        let argv = [
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 1 & exit 0".to_owned(),
        ];
        let output = tokio::time::timeout(
            Duration::from_secs(2),
            run_bounded_command(&argv, Duration::from_secs(2), 4096, None),
        )
        .await
        .expect("a descendant-held pipe must not defeat bounded cleanup")
        .expect("the exited command should be successful");
        assert!(output.status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_runner_keeps_deadline_active_after_both_pipes_close() {
        let argv = [
            "sh".to_owned(),
            "-c".to_owned(),
            "printf out; printf err >&2; sleep 5".to_owned(),
        ];
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            run_bounded_command(&argv, Duration::from_millis(50), 4096, None),
        )
        .await
        .expect("the deadline must remain observable after both readers finish")
        .expect_err("the sleeping child should time out");
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::error::Interrupted>()),
            "timeout should retain its typed interruption: {error:#}"
        );
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_runner_future_kills_readers_and_process_descendants() {
        let marker = crate::test_support::unique_temp_path("lait-process-drop", ".marker");
        let script = format!("(sleep 1; touch '{}') & sleep 5", marker.display());
        let argv = ["sh".to_owned(), "-c".to_owned(), script];
        let mut execution = Box::pin(run_command(&argv, "", None));
        tokio::select! {
            result = &mut execution => panic!("runner unexpectedly completed: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        drop(execution);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "dropping the runner left a process descendant running"
        );
    }
}
