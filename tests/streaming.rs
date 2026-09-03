mod support;

use std::{io::Write, net::TcpListener};

use support::{
    ConfigDirectory, LaitCommand, MockServer, read_request, test_command, without_json_whitespace,
};

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

/// `--stream` combined with `--mcp`: the model's first streamed round ends
/// in a `tool_calls` delta split across several chunks (id/name in one,
/// `arguments` split across two more — the shape a real OpenAI-compatible
/// server streams), which lait must reassemble (see
/// `response::StreamToolCallAccumulator`) before it can call the MCP tool
/// and start a second streamed round for the model's final answer.
#[test]
fn streams_a_round_that_calls_an_mcp_tool_then_streams_the_final_answer() {
    let llm_server = MockServer::start_stream_sequence(&[
        &[
            r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"mock__echo","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"text\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"hi\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
        &[
            r#"{"id":"2","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"the answer is 42"},"finish_reason":null}]}"#,
            r#"{"id":"2","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        ],
    ]);
    let (mcp_url, mcp_thread) = start_mock_mcp_server();
    let config = ConfigDirectory::new(&format!("mcp_servers:\n  mock:\n    url: \"{mcp_url}\"\n"));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--stream",
            "--mcp",
            "mock",
            "what is the answer?",
        ])
        .output()
        .expect("failed to execute lait");

    let first_request = llm_server.receive_request();
    let second_request = llm_server.receive_request();
    llm_server.finish();
    mcp_thread.join().expect("mock MCP server thread panicked");

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "the answer is 42"
    );

    let first_body = without_json_whitespace(&first_request.body);
    assert!(
        first_body.contains(r#""stream":true"#) && first_body.contains(r#""name":"mock__echo""#),
        "first request body: {first_body}"
    );
    let second_body = without_json_whitespace(&second_request.body);
    assert!(
        second_body.contains(r#""role":"tool""#) && second_body.contains("hi"),
        "second request body: {second_body}"
    );
}

/// A regression test for a bug the multi-round streaming tool loop could
/// introduce: `-o` writes each round's content to the same file, and a
/// naive per-round `File::create` would truncate whatever an earlier round
/// already wrote. The first round's own content ("Looking it up... ") must
/// survive into the file alongside the second (final) round's.
#[test]
fn streaming_multiple_rounds_append_to_the_same_output_file() {
    let dir = ConfigDirectory::empty();
    let out_path = dir.path().join("out.txt");

    let llm_server = MockServer::start_stream_sequence(&[
        &[
            r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"Looking it up... "},"finish_reason":null}]}"#,
            r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"mock__echo","arguments":"{\"text\":\"hi\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
        &[
            r#"{"id":"2","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"the answer is 42"},"finish_reason":null}]}"#,
            r#"{"id":"2","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        ],
    ]);
    let (mcp_url, mcp_thread) = start_mock_mcp_server();
    let config = ConfigDirectory::new(&format!("mcp_servers:\n  mock:\n    url: \"{mcp_url}\"\n"));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--stream",
            "--mcp",
            "mock",
            "-o",
        ])
        .arg(&out_path)
        .arg("what is the answer?")
        .output()
        .expect("failed to execute lait");
    llm_server.receive_request();
    llm_server.receive_request();
    llm_server.finish();
    mcp_thread.join().expect("mock MCP server thread panicked");

    assert!(output.status.success(), "lait failed: {output:?}");
    let written = std::fs::read_to_string(&out_path).expect("output file should exist");
    assert_eq!(written, "Looking it up... the answer is 42\n");
}

/// A minimal streamable-HTTP MCP server exposing one `echo` tool — just
/// enough for `streams_a_round_that_calls_an_mcp_tool_then_streams_the_final_answer`,
/// mirroring `tests/mcp.rs`'s own (private, so not reusable across
/// integration test binaries) `start_mock_mcp_server`.
fn start_mock_mcp_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock MCP server");
    let addr = listener
        .local_addr()
        .expect("failed to read mock MCP server address");
    let handle = std::thread::spawn(move || {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().expect("failed to accept connection");
            let request = read_request(&mut stream).expect("failed to read MCP request");
            let request: serde_json::Value =
                serde_json::from_str(&request.body).expect("mock MCP server got non-JSON body");
            let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let id = request.get("id").cloned();

            let (status, response_body) = match method {
                "initialize" => (
                    "200 OK",
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "mock-mcp", "version": "0.0.1"}
                        }
                    })
                    .to_string(),
                ),
                "notifications/initialized" => ("202 Accepted", String::new()),
                "tools/list" => (
                    "200 OK",
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "echo",
                                "description": "echoes the input back",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"text": {"type": "string"}}
                                }
                            }]
                        }
                    })
                    .to_string(),
                ),
                "tools/call" => (
                    "200 OK",
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "42"}]
                        }
                    })
                    .to_string(),
                ),
                other => panic!("mock MCP server received an unexpected method '{other}'"),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len(),
            );
            stream
                .write_all(response.as_bytes())
                .expect("failed to write mock response");
            stream.flush().expect("failed to flush mock response");
        }
    });
    (format!("http://{addr}/mcp"), handle)
}
