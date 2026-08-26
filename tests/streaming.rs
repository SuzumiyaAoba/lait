mod support;

use support::{MockServer, run_lait_with_stream, without_json_whitespace};

#[test]
fn streams_content_deltas_to_stdout() {
    let server = MockServer::start_stream(&[
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"Hello, "},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"world!"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let output = run_lait_with_stream(Some(&server.base_url), Some("test-key"), "hello", false);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(body.contains(r#""stream":true"#), "request body: {body}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Hello, world!"
    );
}

#[test]
fn streams_reasoning_before_content_only_when_show_reasoning_is_set() {
    let events = [
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"reasoning":"step one. "},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}"#,
    ];

    let server = MockServer::start_stream(&events);
    let output = run_lait_with_stream(Some(&server.base_url), None, "hello", false);
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "answer");

    let server = MockServer::start_stream(&events);
    let output = run_lait_with_stream(Some(&server.base_url), None, "hello", true);
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Reasoning:\nstep one. \n\nanswer"
    );
}

#[test]
fn fails_when_the_stream_never_produces_content() {
    let server = MockServer::start_stream(&[
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let output = run_lait_with_stream(Some(&server.base_url), None, "hello", false);
    server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {:?}",
        output
    );
    assert!(!output.stderr.is_empty());
}

#[test]
fn rejects_stream_combined_with_json() {
    let output = support::test_command()
        .args(["--model", "test-model", "--json", "--stream", "hello"])
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
}
