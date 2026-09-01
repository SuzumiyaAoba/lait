mod support;

use support::{JsonSchemaFile, LaitCommand, MockServer, run_lait, without_json_whitespace};

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
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .arg("--json-schema")
        .arg(&schema.path)
        .opt_arg("--schema-name", Some("answer_schema"))
        .prompt("hello")
        .run();
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
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .opt_arg("--reasoning-effort", Some("high"))
        .env("LLM_REASONING_EFFORT", "none")
        .prompt("hello")
        .run();
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
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .opt_arg("--reasoning-effort", Some("none"))
        .prompt("hello")
        .run();
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
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .env("LLM_REASONING_EFFORT", "minimal")
        .prompt("hello")
        .run();
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
fn sends_temperature_top_p_and_max_tokens_when_set() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .opt_arg("--temperature", Some("0.7"))
        .opt_arg("--top-p", Some("0.9"))
        .opt_arg("--max-tokens", Some("256"))
        .prompt("hello")
        .run();
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""temperature":0.7"#),
        "request body: {body}"
    );
    assert!(body.contains(r#""top_p":0.9"#), "request body: {body}");
    assert!(
        body.contains(r#""max_completion_tokens":256"#),
        "request body: {body}"
    );
}

#[test]
fn omits_temperature_top_p_and_max_tokens_when_unset() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .prompt("hello")
        .run();
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(
        !body.contains(r#""temperature""#),
        "request body should omit temperature when unspecified: {body}"
    );
    assert!(
        !body.contains(r#""top_p""#),
        "request body should omit top_p when unspecified: {body}"
    );
    assert!(
        !body.contains(r#""max_completion_tokens""#),
        "request body should omit max_completion_tokens when unspecified: {body}"
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
    assert_eq!(
        output.status.code(),
        Some(4),
        "a model API failure should exit with code 4: {output:?}"
    );
}
