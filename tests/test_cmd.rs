mod support;

use support::{MockServer, ScratchDir, test_command};

fn workflow_yaml(base_url: &str) -> String {
    format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{base_url}"
      model_id: workflow-model
nodes:
  call:
    type: prompt
    prompt: "{{{{ input }}}}"
steps:
  - use: call
"#
    )
}

const OK_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"これは結論です"},"finish_reason":"stop"}]}"#;

/// Builds a scratch directory with `workflow.yml` and a `cassettes/`
/// directory already populated by an actual `lait run --record` against
/// `server` (consuming its one expected connection) — the fixture every test
/// below layers its own `cases/*.yml` test definitions on top of.
fn scratch_with_recorded_cassette() -> ScratchDir {
    let server = MockServer::start("200 OK", OK_BODY);
    let scratch = ScratchDir::new();
    let workflow_path = scratch.write("workflow.yml", &workflow_yaml(&server.base_url));
    let record_dir = scratch.path().join("cassettes");

    let output = test_command()
        .arg("run")
        .arg(&workflow_path)
        .arg("hello")
        .arg("--record")
        .arg(&record_dir)
        .output()
        .expect("failed to execute lait run --record");
    server.receive_request();
    server.finish();
    assert!(output.status.success(), "recording run failed: {output:?}");

    scratch
}

#[test]
fn a_test_definition_with_passing_assertions_reports_pass() {
    let scratch = scratch_with_recorded_cassette();
    scratch.write(
        "cases/pass.yml",
        r#"
workflow: ../workflow.yml
input: "hello"
replay: ../cassettes
assert:
  - type: equals
    value: "これは結論です"
  - type: jq
    expr: 'contains("結論")'
"#,
    );

    let output = test_command()
        .arg("test")
        .arg(scratch.path().join("cases"))
        .output()
        .expect("failed to execute lait test");

    assert!(output.status.success(), "lait test failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PASS"), "{stdout}");
    assert!(stdout.contains("1 passed, 0 failed, 1 total"), "{stdout}");
}

#[test]
fn a_test_definition_with_a_failing_assertion_reports_fail_and_a_nonzero_exit_code() {
    let scratch = scratch_with_recorded_cassette();
    scratch.write(
        "cases/fail.yml",
        r#"
workflow: ../workflow.yml
input: "hello"
replay: ../cassettes
assert:
  - type: jq
    expr: 'contains("nonexistent")'
"#,
    );

    let output = test_command()
        .arg("test")
        .arg(scratch.path().join("cases"))
        .output()
        .expect("failed to execute lait test");

    assert!(
        !output.status.success(),
        "lait test should fail when an assertion fails: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(
        stdout.contains("nonexistent"),
        "expected the failing jq expression to be named in the report: {stdout}"
    );
    assert!(stdout.contains("0 passed, 1 failed, 1 total"), "{stdout}");
}

#[test]
fn recursively_runs_every_test_definition_in_a_directory_and_summarizes() {
    let scratch = scratch_with_recorded_cassette();
    scratch.write(
        "cases/pass.yml",
        r#"
workflow: ../workflow.yml
input: "hello"
replay: ../cassettes
assert:
  - type: jq
    expr: 'contains("結論")'
"#,
    );
    scratch.write(
        "cases/nested/fail.yml",
        r#"
workflow: ../../workflow.yml
input: "hello"
replay: ../../cassettes
assert:
  - type: equals
    value: "unexpected"
"#,
    );

    let output = test_command()
        .arg("test")
        .arg(scratch.path().join("cases"))
        .output()
        .expect("failed to execute lait test");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 passed, 1 failed, 2 total"), "{stdout}");
}

#[test]
fn format_json_reports_a_structured_result_per_file() {
    let scratch = scratch_with_recorded_cassette();
    scratch.write(
        "cases/pass.yml",
        r#"
workflow: ../workflow.yml
input: "hello"
replay: ../cassettes
assert:
  - type: jq
    expr: 'contains("結論")'
"#,
    );

    let output = test_command()
        .arg("test")
        .arg("--format")
        .arg("json")
        .arg(scratch.path().join("cases"))
        .output()
        .expect("failed to execute lait test");

    assert!(output.status.success(), "lait test failed: {output:?}");
    let results: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--format json output should be valid JSON");
    let results = results.as_array().expect("results should be a JSON array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["status"], "pass");
    assert!(results[0]["failures"].as_array().unwrap().is_empty());
}

#[test]
fn a_test_definition_referencing_an_unrecorded_input_fails_with_a_clear_reason() {
    let scratch = scratch_with_recorded_cassette();
    scratch.write(
        "cases/miss.yml",
        r#"
workflow: ../workflow.yml
input: "a completely different prompt never recorded"
replay: ../cassettes
assert:
  - type: equals
    value: "anything"
"#,
    );

    let output = test_command()
        .arg("test")
        .arg(scratch.path().join("cases"))
        .output()
        .expect("failed to execute lait test");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no recorded cassette"), "{stdout}");
}
