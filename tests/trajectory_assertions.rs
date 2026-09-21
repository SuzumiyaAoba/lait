//! Behavioral coverage for `lait test`'s trajectory assertions
//! (`tool_called`/`usage`/`step_output`), added to the shared `assert:`
//! vocabulary in `src/assert.rs` alongside `equals`/`contains`/`jq`/
//! `llm_judge`. `lait eval`'s own (currently unsupported) handling of these
//! is covered separately, inline in `src/assert.rs`'s own unit tests.

mod support;

use support::{ConfigDirectory, MockServer, test_command};

fn tool_call_response(tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"workflow-model","choices":[{{"index":0,"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"{tool}","arguments":"{arguments}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
    )
}

/// The tool round's follow-up response, carrying `usage` so the `usage:`
/// assertion has something non-zero to check.
const FINAL_ANSWER_BODY: &str = r#"{"id":"chatcmpl-2","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;

/// A two-step workflow: `ask` calls the `echo` shell tool then answers
/// "done"; `confirm` is a pure `transform` (no model call) that appends
/// " (confirmed)" — giving a step (`ask`) whose own output differs from the
/// run's final output, so `step_output` assertions actually exercise
/// something distinct from a plain top-level `equals`/`contains`.
fn tools_workflow(base_url: &str) -> String {
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
  ask:
    type: prompt
    prompt: "{{{{ input }}}}"
    tools: [echo]
  confirm:
    type: transform
    jq: '. + " (confirmed)"'
steps:
  - use: ask
  - use: confirm
"#
    )
}

/// Records the two-step tool-using workflow's cassette once, so both tests
/// below can replay it independently.
fn record_cassette(config: &ConfigDirectory, server: &MockServer) {
    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg("workflow.yml")
        .arg("what does echo say?")
        .arg("--record")
        .arg("cassettes")
        .output()
        .expect("failed to execute lait run --record");
    server.receive_request();
    server.receive_request();
    assert!(output.status.success(), "recording run failed: {output:?}");
}

#[test]
fn tool_called_usage_and_step_output_assertions_all_pass() {
    let server = MockServer::start_sequence(&[
        (
            "200 OK",
            &tool_call_response("tool__echo", r#"{\"text\":\"hi\"}"#),
        ),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let config =
        ConfigDirectory::new("tools:\n  echo:\n    command: [\"echo\", \"{{ input.text }}\"]\n");
    config.write("workflow.yml", &tools_workflow(&server.base_url));
    record_cassette(&config, &server);
    server.finish();

    config.write(
        "cases/pass.yml",
        r#"
workflow: ../workflow.yml
input: "what does echo say?"
replay: ../cassettes
assert:
  - type: equals
    value: "done (confirmed)"
  - type: tool_called
    name: tool__echo
    min: 1
    max: 1
    args_jq: '.text == "hi"'
  - type: usage
    max_total_tokens: 100
  - type: step_output
    id: ask
    assert:
      - type: equals
        value: "done"
"#,
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("test")
        .arg("cases")
        .output()
        .expect("failed to execute lait test");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "lait test failed: {output:?}\nstdout: {stdout}"
    );
    assert!(stdout.contains("1 passed, 0 failed, 1 total"), "{stdout}");
}

#[test]
fn trajectory_assertion_failures_report_clear_reasons() {
    let server = MockServer::start_sequence(&[
        (
            "200 OK",
            &tool_call_response("tool__echo", r#"{\"text\":\"hi\"}"#),
        ),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let config =
        ConfigDirectory::new("tools:\n  echo:\n    command: [\"echo\", \"{{ input.text }}\"]\n");
    config.write("workflow.yml", &tools_workflow(&server.base_url));
    record_cassette(&config, &server);
    server.finish();

    config.write(
        "cases/fail.yml",
        r#"
workflow: ../workflow.yml
input: "what does echo say?"
replay: ../cassettes
assert:
  - type: tool_called
    name: tool__never_called
  - type: usage
    max_total_tokens: 1
  - type: step_output
    id: no-such-step
    assert:
      - type: equals
        value: "irrelevant"
"#,
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("test")
        .arg("cases")
        .output()
        .expect("failed to execute lait test");

    assert!(!output.status.success(), "lait test should report a failure");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(
        stdout.contains("at least 1 time(s), was called 0 time(s)"),
        "{stdout}"
    );
    assert!(stdout.contains("total_tokens 15 exceeded"), "{stdout}");
    assert!(stdout.contains("produced no output"), "{stdout}");
}
