use super::*;

#[test]
fn retry_succeeds_after_a_transient_failure() {
    let server = MockServer::start_sequence(&[
        ("500 Internal Server Error", SERVER_ERROR_BODY),
        ("200 OK", CHAT_COMPLETION_BODY),
    ]);
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
  call:
    type: prompt
    prompt: "{{{{ input }}}}"
    retry:
      max_attempts: 2
steps:
  - id: call
    use: call
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn retry_exhausts_all_attempts_and_runs_on_error() {
    let server = MockServer::start_sequence(&[
        ("500 Internal Server Error", SERVER_ERROR_BODY),
        ("500 Internal Server Error", SERVER_ERROR_BODY),
    ]);
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
  call:
    type: prompt
    prompt: "{{{{ input }}}}"
    retry:
      max_attempts: 2
  recover:
    type: transform
    jq: '.input'
steps:
  - id: call
    use: call
    on_error:
      steps:
        - use: recover
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(
        output.status.success(),
        "'on_error' should recover the workflow: {output:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
}

#[test]
fn a_step_without_on_error_fails_the_workflow_after_exhausting_retries() {
    let server = MockServer::start_sequence(&[("500 Internal Server Error", SERVER_ERROR_BODY)]);
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
  echo:
    type: prompt
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "expected the workflow to fail without a 'retry'/'on_error'"
    );
}

#[test]
fn timeout_fails_a_step_whose_attempt_exceeds_the_limit() {
    let server = MockServer::start_delayed(Duration::from_secs(2), "200 OK", CHAT_COMPLETION_BODY);
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
  call:
    type: prompt
    prompt: "{{{{ input }}}}"
    timeout: 1
steps:
  - id: call
    use: call
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    // Not calling server.finish(): the client aborts before the delayed
    // response is written, which would otherwise make the server thread's
    // write fail; only the CLI's own timeout behavior is under test here.

    assert!(
        !output.status.success(),
        "expected a slow attempt past 'timeout' to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "stderr: {stderr}");
    assert_eq!(
        output.status.code(),
        Some(5),
        "a step timeout should exit with code 5: {output:?}"
    );
}

#[test]
fn a_default_retry_recovers_a_step_without_its_own_retry() {
    let server = MockServer::start_sequence(&[
        ("500 Internal Server Error", SERVER_ERROR_BODY),
        ("200 OK", CHAT_COMPLETION_BODY),
    ]);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
  retry:
    max_attempts: 2
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  echo:
    type: prompt
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(
        output.status.success(),
        "the workflow's 'default.retry' should have recovered the step: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}
