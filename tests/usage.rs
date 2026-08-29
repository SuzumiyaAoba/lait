mod support;

use support::{MockServer, WorkflowFile, test_command, without_json_whitespace};

const RESPONSE_WITH_USAGE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":22,"total_tokens":33}}"#;

#[test]
fn show_usage_prints_the_reported_usage_to_stderr() {
    let server = MockServer::start("200 OK", RESPONSE_WITH_USAGE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--show-usage")
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mock response\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("usage: prompt=11 completion=22 total=33"),
        "stderr should carry the usage line: {stderr}"
    );
}

#[test]
fn show_usage_reports_when_the_server_stays_silent() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--show-usage")
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("usage: (the server reported no usage)"),
        "stderr should say usage was not reported: {stderr}"
    );
}

#[test]
fn usage_stays_off_stderr_without_the_flag() {
    let server = MockServer::start("200 OK", RESPONSE_WITH_USAGE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("usage"),
        "stderr should stay quiet without --show-usage: {stderr}"
    );
}

#[test]
fn json_output_includes_the_usage_object() {
    let server = MockServer::start("200 OK", RESPONSE_WITH_USAGE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--json")
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(
        json["usage"],
        serde_json::json!({"prompt_tokens": 11, "completion_tokens": 22, "total_tokens": 33})
    );
}

#[test]
fn streaming_show_usage_requests_and_prints_the_final_chunk_usage() {
    let server = MockServer::start_stream(&[
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":7,"total_tokens":12}}"#,
    ]);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--stream", "--show-usage"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""stream_options":{"include_usage":true}"#),
        "the request should ask for the usage chunk: {body}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "answer");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("usage: prompt=5 completion=7 total=12"),
        "stderr should carry the streamed usage: {stderr}"
    );
}

#[test]
fn workflow_show_usage_prints_a_per_step_summary() {
    let step_response = |content: &str| {
        format!(
            r#"{{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":"{content}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}}}"#
        )
    };
    let first = step_response("first");
    let second = step_response("second");
    let server = MockServer::start_sequence(&[("200 OK", &first), ("200 OK", &second)]);
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
    prompt: "{{{{ input }}}}"
steps:
  - id: step-one
    use: call
  - id: step-two
    use: call
"#,
        server.base_url
    ));

    let output = test_command()
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .arg("--show-usage")
        .output()
        .expect("failed to execute lait run");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("step-one: prompt=10 completion=5 total=15"),
        "stderr should carry step-one's usage: {stderr}"
    );
    assert!(
        stderr.contains("step-two: prompt=10 completion=5 total=15"),
        "stderr should carry step-two's usage: {stderr}"
    );
    assert!(
        stderr.contains("total: prompt=20 completion=10 total=30 (2 requests)"),
        "stderr should carry the total: {stderr}"
    );
}

#[test]
fn streaming_without_show_usage_does_not_request_the_usage_chunk() {
    let server = MockServer::start_stream(&[
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}"#,
    ]);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--stream")
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        !body.contains("stream_options"),
        "the request should not send stream_options: {body}"
    );
}
