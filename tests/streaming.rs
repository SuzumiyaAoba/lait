mod support;

use support::{ConfigDirectory, LaitCommand, MockServer, test_command, without_json_whitespace};

#[test]
fn streams_content_deltas_to_stdout() {
    let server = MockServer::start_stream(&[
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"Hello, "},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"world!"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .api_key(Some("test-key"))
        .arg("--stream")
        .prompt("hello")
        .run();
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
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .arg("--stream")
        .prompt("hello")
        .run();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "answer");

    let server = MockServer::start_stream(&events);
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .arg("--stream")
        .flag_if("--show-reasoning", true)
        .prompt("hello")
        .run();
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
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .arg("--stream")
        .prompt("hello")
        .run();
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
fn stream_appends_default_skills_to_the_system_prompt() {
    let server = MockServer::start_stream(&[
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}"#,
    ]);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\n  skills: [code-review]\nskills:\n  code-review: skill.md\n",
        server.base_url
    ));
    std::fs::write(
        config.path().join("skill.md"),
        "---\n---\nLook for off-by-one errors.\n",
    )
    .expect("failed to write test skill file");

    let output = test_command()
        .current_dir(config.path())
        .args(["--stream", "hello"])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "answer");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"][0],
        serde_json::json!({
            "role": "system",
            "content": "## Skill: code-review\n\nLook for off-by-one errors.",
        })
    );
}

#[test]
fn rejects_stream_combined_with_json() {
    let output = support::test_command()
        .args(["--model", "test-model", "--json", "--stream", "hello"])
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
}
