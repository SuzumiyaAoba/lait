use super::*;

#[test]
fn a_command_node_pipes_the_current_input_to_stdin_and_its_stdout_becomes_the_next_input() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  upper:
    type: command
    command: ["tr", "a-z", "A-Z"]
steps:
  - use: upper
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello world");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "HELLO WORLD"
    );
}

#[test]
fn a_command_nodes_arguments_are_rendered_as_templates() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  greet:
    type: command
    command: ["echo", "hello, {{ input }}"]
steps:
  - use: greet
"#,
    );

    let output = run_lait_workflow(&workflow.path, "world");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello, world"
    );
}

#[test]
fn a_command_node_removes_only_one_trailing_crlf_from_stdout() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  endings:
    type: command
    command: ["printf", "a\r\n\r\n"]
steps:
  - use: endings
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "a\r\n\n");
}

#[test]
fn a_command_nodes_output_flows_into_a_jq_filter_and_the_next_step() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  count:
    type: command
    command: ["wc", "-l"]
    jq: 'tonumber | {{lines: .}}'
  echo:
    type: prompt
    prompt: "{{{{ json input }}}}"
steps:
  - id: count
    use: count
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "a\nb\nc\n");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(request_json["messages"][0]["content"], r#"{"lines":3}"#);
}

#[test]
fn a_commands_nonzero_exit_fails_the_step_with_stderr_in_the_error() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  fail:
    type: command
    command: ["sh", "-c", "echo boom >&2; exit 3"]
steps:
  - use: fail
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(!output.status.success(), "expected the step to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("boom"), "stderr: {stderr}");
}

#[test]
fn a_timed_out_command_is_killed_before_the_workflow_returns() {
    let config = ConfigDirectory::empty();
    let pid_path = config.path().join("timed-out-command.pid");
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  stuck:
    type: command
    command: ["sh", "-c", "echo $$ > '{}'; sleep 5"]
    timeout: 1
steps:
  - use: stuck
"#,
        pid_path.display()
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "hello",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success(), "expected the command to time out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "stderr: {stderr}");
    let pid = std::fs::read_to_string(&pid_path)
        .expect("the command should have written its pid")
        .trim()
        .to_owned();
    let probe = std::process::Command::new("kill")
        .args(["-0", &pid])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to probe the timed-out command");
    assert!(!probe.success(), "timed-out command {pid} is still running");
}

#[cfg(unix)]
#[test]
fn a_timed_out_command_kills_descendants_in_its_process_group() {
    let config = ConfigDirectory::empty();
    let pid_path = config.path().join("timed-out-descendant.pid");
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  stuck:
    type: command
    command: ["sh", "-c", "sleep 5 & echo $! > '{}'; wait"]
    timeout: 1
steps:
  - use: stuck
"#,
        pid_path.display()
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "hello",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success(), "expected the command to time out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "stderr: {stderr}");
    let descendant_pid = std::fs::read_to_string(&pid_path)
        .expect("the command should have written its descendant pid")
        .trim()
        .to_owned();

    // `kill -0` also succeeds for a zombie, so inspect the process state and
    // only consider a non-zombie process to be a leaked descendant. Poll
    // briefly because an orphaned child may take a moment to be reaped by
    // the system's init process after the command's shell is killed.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &descendant_pid])
            .output()
            .expect("failed to inspect the timed-out descendant");
        let state = String::from_utf8_lossy(&state.stdout);
        let running = state
            .trim()
            .chars()
            .next()
            .is_some_and(|state| state != 'Z');
        if !running || Instant::now() >= deadline {
            assert!(
                !running,
                "timed-out descendant {descendant_pid} is still running"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(windows)]
#[test]
fn a_timed_out_command_kills_descendants_in_its_job_object() {
    let config = ConfigDirectory::empty();
    let pid_path = config.path().join("timed-out-descendant.pid");
    // PowerShell accepts forward-slash paths on Windows, which keeps this
    // inline YAML command independent of backslash escape rules.
    let pid_path = pid_path.to_string_lossy().replace('\\', "/");
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  stuck:
    type: command
    command: ["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "$p = Start-Process -FilePath powershell.exe -ArgumentList '-NoLogo','-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 5' -PassThru; Set-Content -LiteralPath '{}' -Value $p.Id; Start-Sleep -Seconds 5"]
    timeout: 1
steps:
  - use: stuck
"#,
        pid_path
    ));

    let started = Instant::now();
    let output = run_lait_workflow(&workflow.path, "hello");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "Windows Job cleanup did not honor the command timeout: {:?}",
        started.elapsed()
    );
    assert!(!output.status.success(), "expected the command to time out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "stderr: {stderr}");

    let descendant_pid = std::fs::read_to_string(config.path().join("timed-out-descendant.pid"))
        .expect("the command should have written its descendant pid")
        .trim()
        .to_owned();
    let filter = format!("PID eq {descendant_pid}");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let tasklist = std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .expect("failed to inspect the timed-out descendant");
        let listed = String::from_utf8_lossy(&tasklist.stdout).contains(&descendant_pid);
        if !listed || Instant::now() >= deadline {
            assert!(
                !listed,
                "timed-out descendant {descendant_pid} is still running"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
#[test]
fn command_timeout_covers_a_blocking_write_file_action() {
    use std::process::Stdio;

    let config = ConfigDirectory::empty();
    let fifo_path = config.path().join("blocked-output.fifo");
    let fifo_status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("failed to create a FIFO for the timeout test");
    assert!(fifo_status.success(), "mkfifo failed: {fifo_status}");
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  emit:
    type: command
    command: ["printf", "done"]
    write_file: "{}"
    timeout: 1
steps:
  - use: emit
"#,
        fifo_path.display()
    ));

    let started = Instant::now();
    let output = test_command()
        .current_dir(config.path())
        .stdin(Stdio::null())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "hello",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(
        started.elapsed() < Duration::from_secs(4),
        "blocking write_file ignored the node timeout: {:?}",
        started.elapsed()
    );
    assert!(!output.status.success(), "expected write_file to time out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "stderr: {stderr}");
}

#[cfg(unix)]
#[test]
fn a_timed_out_write_file_does_not_reach_a_later_on_error_reader() {
    use std::process::Stdio;

    let config = ConfigDirectory::empty();
    let fifo_path = config.path().join("cancelled-output.fifo");
    let fifo_status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("failed to create a FIFO for the cancellation test");
    assert!(fifo_status.success(), "mkfifo failed: {fifo_status}");
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  emit:
    type: transform
    write_file: "{}"
    timeout: 1
  consume:
    type: command
    command: ["sh", "-c", "IFS= read -r value < '{}' && printf 'stale:%s' \"$value\""]
    timeout: 1
  recover:
    type: transform
    jq: '"recovered"'
steps:
  - use: emit
    on_error:
      steps:
        - use: consume
          on_error:
            steps:
              - use: recover
"#,
        fifo_path.display(),
        fifo_path.display()
    ));

    let output = test_command()
        .current_dir(config.path())
        .stdin(Stdio::null())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "hello",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(
        output.status.success(),
        "the nested on_error recovery should succeed: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "recovered",
        "a timed-out write must not be allowed to feed a later reader"
    );
}

#[cfg(unix)]
#[test]
fn a_timed_out_write_file_is_finished_before_a_retry_reuses_the_path() {
    use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, process::Stdio};

    let config = ConfigDirectory::empty();
    let fifo_path = config.path().join("retry-output.fifo");
    let counter_path = config.path().join("attempt.txt");
    create_fifo(&fifo_path);

    // Keep one FIFO descriptor open from just after the first timeout.  A
    // detached writer from that attempt would win this reader and feed it
    // "attempt1"; a cleaned-up writer leaves it for the retry, which must
    // feed "attempt2" instead.
    let reader_path = fifo_path.clone();
    let reader = std::thread::spawn(move || -> std::io::Result<String> {
        std::thread::sleep(Duration::from_millis(1_300));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(reader_path)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut value = String::new();
        let mut buffer = [0_u8; 64];
        while value.is_empty() {
            match file.read(&mut buffer) {
                Ok(0) => {}
                Ok(length) => value.push_str(&String::from_utf8_lossy(&buffer[..length])),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            if !value.is_empty() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(value)
    });

    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  emit:
    type: command
    command: ["sh", "-c", "n=0; test -f '{}' && n=$(cat '{}'); n=$((n+1)); printf '%s' \"$n\" > '{}'; printf 'attempt%s' \"$n\""]
    write_file: "{}"
    timeout: 1
    retry:
      max_attempts: 2
      delay_seconds: 1
steps:
  - use: emit
"#,
        counter_path.display(),
        counter_path.display(),
        counter_path.display(),
        fifo_path.display()
    ));

    let output = test_command()
        .current_dir(config.path())
        .stdin(Stdio::null())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "hello",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");
    let read_value = reader
        .join()
        .expect("FIFO reader thread should not panic")
        .expect("FIFO reader should receive the retry output");

    assert!(
        output.status.success(),
        "a retry should succeed after the timed-out writer is cleaned up: {output:?}"
    );
    assert_eq!(
        read_value, "attempt2",
        "the reader must not receive bytes from the timed-out attempt"
    );
}

#[test]
fn command_does_not_wait_for_a_stdin_writer_after_the_child_exits() {
    use std::{io::Write, process::Stdio};

    let workflow = WorkflowFile::new(
        r#"
nodes:
  exits:
    type: command
    command: ["sh", "-c", "sleep 5 >/dev/null 2>/dev/null & exit 0"]
steps:
  - use: exits
"#,
    );
    let mut command = test_command();
    command
        .args(["run", workflow.path.to_str().unwrap(), "-", "--no-history"])
        .stdin(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn lait");
    let started = Instant::now();
    child
        .stdin
        .take()
        .expect("lait stdin should be piped")
        .write_all(&vec![b'x'; 1024 * 1024])
        .expect("failed to write test input");
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait to finish");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "lait waited for a descendant-held stdin pipe: {:?}",
        started.elapsed()
    );
}

#[test]
fn command_timeout_interrupts_reader_tasks_after_the_child_exits() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  exits:
    type: command
    command: ["sh", "-c", "sleep 5 & exit 0"]
    timeout: 1
steps:
  - use: exits
"#,
    );
    let started = Instant::now();
    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(
        started.elapsed() < Duration::from_secs(4),
        "reader tasks ignored the node timeout: {:?}",
        started.elapsed()
    );
    assert!(!output.status.success(), "expected the command to time out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "stderr: {stderr}");
}

#[cfg(unix)]
#[test]
fn a_timed_out_jq_worker_is_reaped_before_on_error_and_cannot_write_output() {
    let config = ConfigDirectory::empty();
    let output_path = config.path().join("jq-timeout-output.txt");
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  spin:
    type: transform
    jq: 'range(0; 1000000000)'
    write_file: "{}"
    timeout: 1
  recover:
    type: transform
    jq: '"recovered"'
steps:
  - use: spin
    on_error:
      steps:
        - use: recover
"#,
        output_path.display()
    ));

    let started = Instant::now();
    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "timed-out jq evaluation delayed on_error recovery: {:?}",
        started.elapsed()
    );
    assert!(
        output.status.success(),
        "on_error should run after the jq worker is cancelled: {output:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "recovered");
    assert!(
        !output_path.exists(),
        "write_file must not run after jq cancellation"
    );
}

#[test]
fn a_nonzero_command_exit_can_be_caught_by_on_error() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  fail:
    type: command
    command: ["sh", "-c", "exit 1"]
  recover:
    type: transform
    jq: '"recovered"'
steps:
  - use: fail
    on_error:
      steps:
        - use: recover
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(
        output.status.success(),
        "on_error should have recovered the failing command: {output:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "recovered");
}

#[test]
fn an_empty_command_list_is_a_clear_lint_error() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  n:
    type: command
    command: []
steps:
  - use: n
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("empty 'command'"), "stderr: {stderr}");
}
