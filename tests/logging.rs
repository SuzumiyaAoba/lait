mod support;

use support::{LaitCommand, MockServer};

#[test]
fn without_verbose_no_trace_output_appears_on_stderr() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
    );
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .api_key(Some("test-key"))
        .prompt("hello")
        .run();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.is_empty(), "unexpected stderr without -v: {stderr}");
}

#[test]
fn dash_v_v_traces_the_request_on_stderr_without_touching_stdout() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
    );
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .api_key(Some("test-super-secret-key"))
        .arg("-vv")
        .prompt("hello")
        .run();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response",
        "stdout must stay pipe-clean even with -vv"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolved request settings"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("sending completion request"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("received completion response"),
        "stderr: {stderr}"
    );
}

#[test]
fn dash_v_v_masks_the_api_key_on_stderr() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
    );
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .api_key(Some("test-super-secret-key"))
        .arg("-vv")
        .prompt("hello")
        .run();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("test-super-secret-key"),
        "the raw api key must never appear in verbose output: {stderr}"
    );
    assert!(
        stderr.contains("test***"),
        "expected the masked api key prefix in stderr: {stderr}"
    );
}
