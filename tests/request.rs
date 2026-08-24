mod support;

use support::{
    JsonSchemaFile, MockServer, run_lait, run_lait_with_json_schema, run_lait_with_request_options,
    without_json_whitespace,
};

#[test]
fn sends_prompt_to_openai_compatible_chat_completions() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
    );
    let output = run_lait(Some(&server.base_url), Some("test-key"), "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key")
    );

    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"test-model""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""messages":[{"role":"user","content":"hello"}]"#),
        "request body: {body}"
    );
    assert!(body.contains(r#""stream":false"#), "request body: {body}");
    assert!(
        !body.contains(r#""reasoning_effort""#),
        "request body should omit reasoning_effort when unspecified: {body}"
    );
    assert!(
        !body.contains(r#""response_format""#),
        "request body should omit response_format when unspecified: {body}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn sends_strict_json_schema_response_format() {
    let schema = JsonSchemaFile::new(
        r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#,
    );
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"answer\":\"mock response\"}"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_json_schema(
        Some(&server.base_url),
        None,
        "hello",
        &schema.path,
        Some("answer_schema"),
    );
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["response_format"],
        serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "answer_schema",
                "schema": {
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                    "additionalProperties": false,
                },
                "strict": true,
            },
        })
    );
}

#[test]
fn cli_reasoning_effort_overrides_environment() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_request_options(
        Some(&server.base_url),
        None,
        "hello",
        false,
        Some("high"),
        Some("none"),
    );
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""reasoning_effort":"high""#),
        "request body: {body}"
    );
}

#[test]
fn sends_none_reasoning_effort_when_explicitly_requested() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_request_options(
        Some(&server.base_url),
        None,
        "hello",
        false,
        Some("none"),
        None,
    );
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""reasoning_effort":"none""#),
        "request body: {body}"
    );
}

#[test]
fn sends_reasoning_effort_from_environment() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_request_options(
        Some(&server.base_url),
        None,
        "hello",
        false,
        None,
        Some("minimal"),
    );
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""reasoning_effort":"minimal""#),
        "request body: {body}"
    );
}

#[test]
fn reports_openai_api_errors() {
    let server = MockServer::start(
        "500 Internal Server Error",
        r#"{"error":{"message":"mock failure","type":"server_error"}}"#,
    );
    let output = run_lait(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {:?}",
        output
    );
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        !output.stderr.is_empty(),
        "API errors should be reported on stderr"
    );
}
