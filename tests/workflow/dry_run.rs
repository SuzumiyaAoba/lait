use super::*;

#[cfg(unix)]
#[test]
fn dry_run_does_not_run_an_api_key_command() {
    let config = ConfigDirectory::empty();
    let marker = config.path().join("api-key-command-ran");
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: http://127.0.0.1:1/v1
        api_key_cmd: ["sh", "-c", "touch '{}' ; printf dry-run-secret"]
      model_id: workflow-model
nodes:
  echo:
    type: prompt
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
"#,
        marker.display()
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "run",
            workflow
                .path
                .to_str()
                .expect("workflow path should be UTF-8"),
            "hello",
            "--dry-run",
        ])
        .output()
        .expect("failed to execute lait run --dry-run");

    assert!(output.status.success(), "dry-run failed: {output:?}");
    assert!(
        !marker.exists(),
        "dry-run must not resolve an API-key command"
    );
}

#[test]
fn dry_run_prints_the_plan_without_calling_a_model() {
    // No server is started at all: if `--dry-run` ever attempted a real
    // completion against this unreachable address, the run would fail
    // (connection refused) instead of exiting successfully.
    let workflow = WorkflowFile::new(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: http://127.0.0.1:1/v1
      model_id: workflow-model
nodes:
  extract:
    type: prompt
    prompt: "extract from: {{ input }}"
    retry:
      max_attempts: 3
      delay_seconds: 1
      backoff: 2.0
    timeout: 30
  greet:
    type: prompt
    prompt: "city was {{ steps.extract.city }}"
steps:
  - id: extract
    use: extract
  - use: greet
"#,
    );

    let output = test_command()
        .args([
            "run",
            workflow.path.to_str().expect("workflow path is utf-8"),
            "hello world",
            "--dry-run",
        ])
        .output()
        .expect("failed to execute lait run --dry-run");

    assert!(
        output.status.success(),
        "lait run --dry-run failed: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("extract from: hello world"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("workflow-model @ http://127.0.0.1:1/v1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("retry: max_attempts=3, delay_seconds=1, backoff=2"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("timeout: 30s"), "stdout: {stdout}");
    assert!(
        stdout.contains("[unrendered:"),
        "a template referencing an earlier step's output should stay unrendered: {stdout}"
    );
}

#[test]
fn dry_run_reports_a_missing_model_without_calling_anything() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  greet:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: greet
"#,
    );

    let output = test_command()
        .args([
            "run",
            workflow.path.to_str().expect("workflow path is utf-8"),
            "hello",
            "--dry-run",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run --dry-run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("model is required"), "stderr: {stderr}");
}
