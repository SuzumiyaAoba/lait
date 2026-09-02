mod support;

use std::fs;
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{ConfigDirectory, test_command};

/// Sends SIGINT directly to `pid` — the same signal a terminal's Ctrl-C
/// delivers to its foreground process, approximated here without needing an
/// actual controlling terminal in the test harness.
fn send_sigint(pid: u32) {
    // SAFETY: `pid` is a live child we own (`Child::id()`), and `kill(2)`
    // with SIGINT has no memory-safety preconditions beyond a valid pid.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGINT);
    }
}

#[test]
fn sigint_cancels_a_running_workflow_and_saves_a_checkpoint() {
    let dir = ConfigDirectory::empty();
    fs::write(
        dir.path().join("workflow.yml"),
        r#"
nodes:
  mark:
    type: command
    command: ["sh", "-c", "echo ran >> marker.txt; cat"]
  slow:
    type: command
    command: ["sleep", "5"]
steps:
  - use: mark
  - use: slow
"#,
    )
    .expect("failed to write test workflow");

    let child = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "hello",
            "--checkpoint",
            "--no-config",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn lait run");

    // Give the process time to start (dynamic linking + the tokio runtime
    // + `signal::spawn_handler` registering its SIGINT handler — under a
    // loaded/cold test harness this alone can take over a second, and a
    // signal sent before the handler is registered just kills the process
    // outright instead of exercising it), step 1 (trivial) time to finish,
    // and step 2 (the 5s sleep) time to actually start.
    std::thread::sleep(Duration::from_millis(2000));
    send_sigint(child.id());

    let started_waiting = Instant::now();
    // `wait_with_output` drains stdout/stderr concurrently with waiting, so
    // it can't deadlock against a full pipe buffer the way manually reading
    // one stream to completion before calling `wait()` could.
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait run");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The whole point of graceful cancellation: exit well before the 5s
    // sleep would have finished on its own.
    assert!(
        started_waiting.elapsed() < Duration::from_secs(4),
        "lait run did not exit promptly after SIGINT (took {:?})",
        started_waiting.elapsed()
    );
    assert_eq!(output.status.code(), Some(130), "stderr: {stderr}");
    assert!(stderr.contains("received Ctrl-C"), "stderr: {stderr}");
    assert!(stderr.contains("run checkpointed as"), "stderr: {stderr}");

    let runs_dir = dir.path().join(".lait/runs");
    let entries: Vec<_> = fs::read_dir(&runs_dir)
        .expect("failed to read .lait/runs")
        .filter_map(|entry| entry.ok())
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one checkpoint file");
    let checkpoint: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(entries[0].path()).expect("failed to read checkpoint file"),
    )
    .expect("checkpoint file was not valid JSON");
    assert_eq!(checkpoint["status"], "failed");
    assert_eq!(checkpoint["completed_index"], 1);
    assert_eq!(checkpoint["current_input"], "hello");
}

#[test]
fn workflow_timeout_cancels_a_run_that_exceeds_the_budget() {
    let dir = ConfigDirectory::empty();
    fs::write(
        dir.path().join("workflow.yml"),
        r#"
default:
  workflow_timeout: 1
nodes:
  slow:
    type: command
    command: ["sleep", "10"]
steps:
  - use: slow
"#,
    )
    .expect("failed to write test workflow");

    let started = Instant::now();
    let output = test_command()
        .current_dir(dir.path())
        .args(["run", "workflow.yml", "hello", "--no-config"])
        .output()
        .expect("failed to execute lait run");
    let elapsed = started.elapsed();

    assert!(!output.status.success());
    // Must fail near the 1s budget, not wait out the 10s sleep.
    assert!(
        elapsed < Duration::from_secs(5),
        "workflow_timeout did not cut the run short (took {elapsed:?})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cancelled"), "stderr: {stderr}");
    assert!(
        stderr.contains("workflow_timeout"),
        "stderr should name the budget that was hit: {stderr}"
    );
    // A real Ctrl-C was never sent, so this must not be misclassified as one.
    assert_ne!(output.status.code(), Some(130), "stderr: {stderr}");
}

#[test]
fn workflow_timeout_rejects_a_zero_value() {
    let dir = ConfigDirectory::empty();
    fs::write(
        dir.path().join("workflow.yml"),
        "default:\n  workflow_timeout: 0\nnodes:\n  echo:\n    type: transform\n    jq: '.'\nsteps:\n  - use: echo\n",
    )
    .expect("failed to write test workflow");

    let output = test_command()
        .current_dir(dir.path())
        .args(["run", "workflow.yml", "hello", "--no-config"])
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("workflow_timeout"), "stderr: {stderr}");
}
