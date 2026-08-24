mod support;

use support::{MockServer, run_lait, run_lait_with_json, run_lait_with_options};

#[test]
fn hides_reasoning_without_show_reasoning_option() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"internal reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mock response\n");
}

#[test]
fn shows_reasoning_with_show_reasoning_option() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"internal reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_options(Some(&server.base_url), None, "hello", true);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Reasoning:\ninternal reasoning\n\nmock response\n"
    );
}

#[test]
fn shows_legacy_reasoning_content_with_show_reasoning_option() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning_content":"internal reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_options(Some(&server.base_url), None, "hello", true);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Reasoning:\ninternal reasoning\n\nmock response\n"
    );
}

#[test]
fn shows_only_final_content_when_reasoning_content_is_missing() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_options(Some(&server.base_url), None, "hello", true);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mock response\n");
}

#[test]
fn emits_json_with_null_reasoning_when_reasoning_is_missing() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock \"response\"\nsecond line"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_json(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(
        json,
        serde_json::json!({
            "content": "mock \"response\"\nsecond line",
            "reasoning": null,
        })
    );
}

#[test]
fn emits_json_with_current_reasoning_in_preference_to_legacy_reasoning_content() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"current reasoning","reasoning_content":"legacy reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_json(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(
        json,
        serde_json::json!({
            "content": "mock response",
            "reasoning": "current reasoning",
        })
    );
}

#[test]
fn emits_json_with_legacy_reasoning_content_when_current_reasoning_is_blank() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"  ","reasoning_content":"legacy reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_json(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(
        json,
        serde_json::json!({
            "content": "mock response",
            "reasoning": "legacy reasoning",
        })
    );
}
