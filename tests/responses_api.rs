//! Behavioral coverage for `models.<alias>[0].api: responses`
//! (`src/config/types.rs`'s `ApiKind`, `src/llm/responses.rs`'s wire
//! translation, `src/engine/transport.rs`'s dispatch/guards): a model
//! definition can opt into OpenAI's Responses API (`POST /responses`)
//! instead of Chat Completions (`POST /chat/completions`), with the reply
//! translated back into the exact same `response::ChatCompletionResponse`
//! shape every other path produces.

mod support;

use support::{ConfigDirectory, MockServer, test_command, without_json_whitespace};

fn responses_api_body(text: &str) -> String {
    format!(
        r#"{{"id":"resp_1","object":"response","status":"completed","output":[{{"type":"reasoning","summary":[]}},{{"type":"message","id":"msg_1","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}],"usage":{{"input_tokens":12,"output_tokens":4,"total_tokens":16}}}}"#
    )
}

fn responses_model_config(base_url: &str) -> String {
    format!(
        "default:\n  model: cloud\nmodels:\n  cloud:\n    - provider:\n        base_url: \"{base_url}\"\n      model_id: gpt-5-reasoning\n      api: responses\n"
    )
}

#[test]
fn a_responses_api_model_sends_a_translated_request_and_renders_the_reply() {
    let server = MockServer::start(
        "200 OK",
        &responses_api_body("hello from the responses api"),
    );
    let config = ConfigDirectory::new(&responses_model_config(&server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello from the responses api"
    );
    assert_eq!(request.target, "/v1/responses", "request: {request:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"gpt-5-reasoning""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""input":[{"role":"user","content":"hello"}]"#),
        "request body: {body}"
    );
    assert!(body.contains(r#""store":false"#), "request body: {body}");
    assert!(
        !body.contains("previous_response_id"),
        "request body: {body}"
    );
}

#[test]
fn a_responses_api_model_carries_the_system_prompt_as_instructions() {
    let server = MockServer::start("200 OK", &responses_api_body("ok"));
    let config = ConfigDirectory::new(&responses_model_config(&server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["--system", "be terse", "hello"])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(
        request.body.contains(r#""instructions":"be terse""#),
        "request body: {}",
        request.body
    );
    assert!(
        !request.body.contains(r#""role":"system""#),
        "the system prompt must not also appear as an 'input' item: {}",
        request.body
    );
}

#[test]
fn a_responses_api_model_drives_a_shell_tool_loop() {
    let server = MockServer::start_sequence(&[
        (
            "200 OK",
            r#"{"status":"completed","output":[{"type":"function_call","call_id":"call_1","name":"tool__echo","arguments":"{}"}]}"#,
        ),
        ("200 OK", &responses_api_body("tool said hi")),
    ]);
    let mut config_yaml = responses_model_config(&server.base_url);
    config_yaml.push_str("tools:\n  echo:\n    command: [\"echo\", \"hi\"]\n");
    let config = ConfigDirectory::new(&config_yaml);

    let output = test_command()
        .current_dir(config.path())
        .args(["--tool", "echo", "hello"])
        .output()
        .expect("failed to execute lait");
    let first = server.receive_request();
    let second = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "tool said hi"
    );
    let first_body = without_json_whitespace(&first.body);
    assert!(
        first_body.contains(r#""tools":[{"type":"function","name":"tool__echo""#),
        "first request body: {first_body}"
    );
    let second_body = without_json_whitespace(&second.body);
    assert!(
        second_body.contains(
            r#"{"type":"function_call","call_id":"call_1","name":"tool__echo","arguments":"{}"}"#
        ),
        "second request body: {second_body}"
    );
    assert!(
        second_body.contains(r#""type":"function_call_output","call_id":"call_1","output":"hi"#),
        "second request body: {second_body}"
    );
}

#[test]
fn a_responses_api_model_streams_output_text_deltas() {
    let server = MockServer::start_stream(&[
        r#"{"type":"response.created","response":{"status":"in_progress"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"Hello, "}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"world!"}"#,
        r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#,
    ]);
    let config = ConfigDirectory::new(&responses_model_config(&server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["--stream", "--show-usage", "hello"])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(request.target, "/v1/responses", "request: {request:?}");
    let body = without_json_whitespace(&request.body);
    assert!(body.contains(r#""stream":true"#), "request body: {body}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Hello, world!"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("total=5"), "stderr: {stderr}");
}

#[test]
fn a_responses_api_stream_failure_surfaces_the_servers_error_message() {
    let server = MockServer::start_stream(&[
        r#"{"type":"response.failed","response":{"status":"failed","error":{"message":"rate_limited"}}}"#,
    ]);
    let config = ConfigDirectory::new(&responses_model_config(&server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["--stream", "hello"])
        .output()
        .expect("failed to execute lait");
    server.finish();

    assert!(!output.status.success(), "output: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rate_limited"), "stderr: {stderr}");
}

#[test]
fn a_responses_api_model_sends_image_attachments_as_input_image_parts() {
    let server = MockServer::start("200 OK", &responses_api_body("a cat"));
    let config = ConfigDirectory::new(&responses_model_config(&server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["--image", "http://example.com/x.png", "hello"])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(
            r#""content":[{"type":"input_text","text":"hello"},{"type":"input_image","image_url":"http://example.com/x.png"}]"#
        ),
        "request body: {body}"
    );
}

#[test]
fn a_failed_responses_api_status_surfaces_the_servers_error_message() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"resp_1","object":"response","status":"failed","output":[],"error":{"message":"insufficient_quota"}}"#,
    );
    let config = ConfigDirectory::new(&responses_model_config(&server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.finish();

    assert!(!output.status.success(), "output: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("insufficient_quota"), "stderr: {stderr}");
}

#[test]
fn cache_and_replay_work_for_a_responses_api_model() {
    let server = MockServer::start("200 OK", &responses_api_body("cached reply"));
    let config = ConfigDirectory::new(&responses_model_config(&server.base_url));

    let first = test_command()
        .current_dir(config.path())
        .args(["--cache", "hello"])
        .output()
        .expect("failed to execute lait");
    server.receive_request();

    let second = test_command()
        .current_dir(config.path())
        .args(["--cache", "hello"])
        .output()
        .expect("failed to execute lait");
    // No second request: `--cache` must have served the reply from disk.
    let leaked = server.try_receive_request(std::time::Duration::from_millis(300));
    server.finish();

    assert!(first.status.success(), "first run failed: {first:?}");
    assert!(second.status.success(), "second run failed: {second:?}");
    assert_eq!(
        String::from_utf8_lossy(&first.stdout).trim(),
        "cached reply"
    );
    assert_eq!(
        String::from_utf8_lossy(&second.stdout).trim(),
        "cached reply"
    );
    assert!(
        leaked.is_none(),
        "a cache hit must not send a second request"
    );
}

#[test]
fn a_responses_api_models_fallback_candidate_uses_chat_completions() {
    let primary = MockServer::start(
        "503 Service Unavailable",
        r#"{"error":{"message":"mock outage","type":"server_error"}}"#,
    );
    let secondary = MockServer::start(
        "200 OK",
        &support::completion_body("model-b", "from the chat completions fallback"),
    );
    let config = ConfigDirectory::new(&format!(
        "default:\n  model: multi\nmodels:\n  multi:\n    - provider:\n        base_url: \"{}\"\n      model_id: model-a\n      api: responses\n    - provider:\n        base_url: \"{}\"\n      model_id: model-b\n",
        primary.base_url, secondary.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    while primary
        .try_receive_request(std::time::Duration::from_secs(2))
        .is_some()
    {}
    let second_request = secondary.receive_request();
    primary.finish();
    secondary.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "from the chat completions fallback"
    );
    assert_eq!(
        second_request.target, "/v1/chat/completions",
        "the fallback candidate must use Chat Completions regardless of the primary's 'api:': \
         {second_request:?}"
    );
}
