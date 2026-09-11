// `tests/workflow/main.rs` (this file) is one directory deeper than every
// other integration test binary's crate root, so the default sibling-file
// module resolution `mod support;` would otherwise look for
// `tests/workflow/support.rs` instead of the real, shared `tests/support/`.
#[path = "../support/mod.rs"]
mod support;

use std::time::{Duration, Instant};

#[cfg(unix)]
use std::path::Path;

use support::{
    AgentMarkdownFile, ConfigDirectory, JsonSchemaFile, MINIMAL_PNG_BYTES, MockServer,
    WorkflowFile, run_lait_workflow, test_command, without_json_whitespace,
};

const SERVER_ERROR_BODY: &str = r#"{"error":{"message":"mock failure","type":"server_error"}}"#;

const CHAT_COMPLETION_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#;

#[cfg(unix)]
fn create_fifo(path: &Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("failed to create FIFO for timeout test");
    assert!(status.success(), "mkfifo failed: {status}");
}

#[cfg(unix)]
fn timeout_workflow(node_fields: &str) -> WorkflowFile {
    WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: http://127.0.0.1:1/v1
      model_id: workflow-model
nodes:
  call:
{node_fields}
steps:
  - use: call
"#,
    ))
}

#[cfg(unix)]
fn run_workflow_until_timeout(workflow: &Path) -> (std::process::Output, Duration) {
    use std::process::Stdio;

    let started = Instant::now();
    let mut child = test_command()
        .args([
            "run",
            workflow
                .to_str()
                .expect("workflow path should be valid UTF-8"),
            "hello",
            "--no-history",
            "--no-config",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn timed workflow");
    let deadline = started + Duration::from_secs(3);

    loop {
        if child
            .try_wait()
            .expect("failed to poll timed workflow")
            .is_some()
        {
            let elapsed = started.elapsed();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed workflow output");
            return (output, elapsed);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to reap a hung timed workflow");
            panic!(
                "timed workflow did not return after cancellation: elapsed={:?}, output={output:?}",
                started.elapsed()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn assert_fifo_read_times_out(workflow: &Path) {
    let (output, elapsed) = run_workflow_until_timeout(workflow);
    assert!(
        elapsed < Duration::from_secs(3),
        "workflow timeout did not return promptly: {elapsed:?}"
    );
    assert!(
        !output.status.success(),
        "expected the blocked workflow to time out: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timed out"),
        "expected a timeout error, stderr: {stderr}"
    );
}

mod agents;
mod attachments;
mod command;
mod concurrency;
mod control_flow;
mod dry_run;
mod env_vars;
mod fifo_cancellation;
mod loops;
mod prompts;
mod retry_timeout;
mod routers;
mod run_vars;
mod schemas;
mod settings;
mod step_outputs;
mod subworkflow;
mod workflow_timeout;
mod write_file;
