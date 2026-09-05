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

const OK_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"recorded answer"},"finish_reason":"stop"}]}"#;

#[test]
fn record_writes_a_cassette_file_for_the_request() {
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

    assert!(
        output.status.success(),
        "lait run --record failed: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "recorded answer"
    );
    let entries: Vec<_> = std::fs::read_dir(&record_dir)
        .expect("record dir should have been created")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one cassette file, found {entries:?}"
    );
}

/// After the record phase above, the mock server's background thread has
/// already returned from its single expected `accept()` (see
/// `support::MockServer::start`/`start_sequence`'s doc comment) — so if
/// `--replay` incorrectly tried the network here, the connection would be
/// refused (nothing is listening any more) and this run would fail loudly,
/// rather than silently succeeding. That failure mode is what actually
/// proves "no network access", not a `try_receive_request` check against a
/// server whose listener has already been dropped.
#[test]
fn replay_answers_from_a_previously_recorded_cassette_without_contacting_the_network() {
    let server = MockServer::start("200 OK", OK_BODY);
    let scratch = ScratchDir::new();
    let workflow_path = scratch.write("workflow.yml", &workflow_yaml(&server.base_url));
    let record_dir = scratch.path().join("cassettes");

    let record_output = test_command()
        .arg("run")
        .arg(&workflow_path)
        .arg("hello")
        .arg("--record")
        .arg(&record_dir)
        .output()
        .expect("failed to execute lait run --record");
    server.receive_request();
    server.finish();
    assert!(
        record_output.status.success(),
        "recording run failed: {record_output:?}"
    );

    let replay_output = test_command()
        .arg("run")
        .arg(&workflow_path)
        .arg("hello")
        .arg("--replay")
        .arg(&record_dir)
        .output()
        .expect("failed to execute lait run --replay");

    assert!(
        replay_output.status.success(),
        "replay run failed: {replay_output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&replay_output.stdout).trim(),
        "recorded answer"
    );
}

#[test]
fn replay_fails_clearly_for_an_unrecorded_request() {
    let server = MockServer::start("200 OK", OK_BODY);
    let scratch = ScratchDir::new();
    let workflow_path = scratch.write("workflow.yml", &workflow_yaml(&server.base_url));
    let record_dir = scratch.path().join("cassettes");

    let record_output = test_command()
        .arg("run")
        .arg(&workflow_path)
        .arg("hello")
        .arg("--record")
        .arg(&record_dir)
        .output()
        .expect("failed to execute lait run --record");
    server.receive_request();
    server.finish();
    assert!(
        record_output.status.success(),
        "recording run failed: {record_output:?}"
    );

    // A different prompt hashes to a different cassette key, so this must
    // miss — and must fail rather than silently falling back to the network
    // (which, per the doc comment on the previous test, would surface as a
    // connection-refused failure with a *different* message than the one
    // asserted below).
    let replay_output = test_command()
        .arg("run")
        .arg(&workflow_path)
        .arg("goodbye")
        .arg("--replay")
        .arg(&record_dir)
        .output()
        .expect("failed to execute lait run --replay");

    assert!(
        !replay_output.status.success(),
        "replay of an unrecorded request should fail: {replay_output:?}"
    );
    let stderr = String::from_utf8_lossy(&replay_output.stderr);
    assert!(
        stderr.contains("no recorded cassette"),
        "expected a clear 'no recorded cassette' error: {stderr}"
    );
    assert!(
        stderr.contains("--record"),
        "expected a hint to re-record: {stderr}"
    );
}

#[test]
fn record_and_replay_are_mutually_exclusive() {
    let scratch = ScratchDir::new();
    let workflow_path = scratch.write("workflow.yml", &workflow_yaml("http://127.0.0.1:1"));

    let output = test_command()
        .arg("run")
        .arg(&workflow_path)
        .arg("hello")
        .arg("--record")
        .arg("a")
        .arg("--replay")
        .arg("b")
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success());
}
