mod support;

use std::io::Write;
use std::net::TcpListener;
use std::time::{Duration, Instant};

use support::{
    ConfigDirectory, MockServer, WorkflowFile, read_request, test_command, without_json_whitespace,
};

const CHAT_COMPLETION_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#;

/// A hand-rolled streamable-HTTP MCP server for integration tests: routes on
/// the JSON-RPC `method` field (something `support::MockServer` can't do,
/// since it just replays canned bodies in connection order) and answers
/// `initialize` / `notifications/initialized` / `tools/list` / `tools/call` —
/// the four requests one tool-call round trip makes. Exposes a single tool,
/// `echo`, that always returns a fixed string regardless of its arguments.
/// Request parsing itself reuses `support::read_request`, the same raw
/// header/`Content-Length`/body reader `support::MockServer` uses.
fn start_mock_mcp_server() -> (String, std::thread::JoinHandle<()>) {
    start_mock_mcp_server_with_list_cursor(None)
}

/// Starts the same test server but includes `nextCursor` in every
/// `tools/list` response when `next_cursor` is `Some`. This exercises the
/// client's pagination safety checks without involving an LLM request: the
/// repeated cursor must be rejected while listing tools.
fn start_mock_mcp_server_with_list_cursor(
    next_cursor: Option<&'static str>,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock MCP server");
    let addr = listener
        .local_addr()
        .expect("failed to read mock MCP server address");
    let handle = std::thread::spawn(move || {
        let mut expected_cursor = None;
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().expect("failed to accept connection");
            let request = read_request(&mut stream).expect("failed to read MCP request");
            let request: serde_json::Value =
                serde_json::from_str(&request.body).expect("mock MCP server got non-JSON body");
            let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let id = request.get("id").cloned();
            if method == "tools/list"
                && let Some(expected_cursor) = expected_cursor
            {
                let actual_cursor = request
                    .get("params")
                    .and_then(|params| params.get("cursor"))
                    .and_then(serde_json::Value::as_str);
                assert_eq!(actual_cursor, Some(expected_cursor));
            }

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
                "tools/list" => {
                    let mut body = serde_json::json!({
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
                    });
                    if let Some(next_cursor) = next_cursor {
                        body["result"]["nextCursor"] = serde_json::json!(next_cursor);
                        expected_cursor = Some(next_cursor);
                    }
                    ("200 OK", body.to_string())
                }
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

/// Starts a server whose first `tools/list` pagination fails with a repeated
/// cursor, then serves a fresh connection normally. The listener is
/// non-blocking so the test can also finish when a buggy client retries on the
/// stale connection and never sends the second handshake.
fn start_recovering_mcp_server() -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock MCP server");
    listener
        .set_nonblocking(true)
        .expect("failed to make mock MCP server non-blocking");
    let addr = listener
        .local_addr()
        .expect("failed to read mock MCP server address");
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut methods = Vec::new();
        let mut list_count = 0usize;
        while Instant::now() < deadline && methods.len() < 7 {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("failed to accept connection: {error}"),
            };
            let request = read_request(&mut stream).expect("failed to read MCP request");
            let request: serde_json::Value =
                serde_json::from_str(&request.body).expect("mock MCP server got non-JSON body");
            let method = request
                .get("method")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            let id = request.get("id").cloned();
            methods.push(method.clone());

            let (status, response_body) = match method.as_str() {
                "initialize" => (
                    "200 OK",
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "recovering-mcp", "version": "0.0.1"}
                        }
                    })
                    .to_string(),
                ),
                "notifications/initialized" => ("202 Accepted", String::new()),
                "tools/list" => {
                    list_count += 1;
                    let next_cursor = (list_count <= 2).then_some("same");
                    let mut body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "echo",
                                "description": "echoes the input back",
                                "inputSchema": {"type": "object"}
                            }]
                        }
                    });
                    if let Some(next_cursor) = next_cursor {
                        body["result"]["nextCursor"] = serde_json::json!(next_cursor);
                    }
                    ("200 OK", body.to_string())
                }
                other => panic!("recovering MCP server received unexpected method '{other}'"),
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
        methods
    });
    (format!("http://{addr}/mcp"), handle)
}

#[test]
fn chat_mode_calls_an_mcp_tool_and_returns_the_models_final_answer() {
    let llm_server = MockServer::start_sequence(&[
        (
            "200 OK",
            r#"{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"mock__echo","arguments":"{\"text\":\"hi\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ),
        (
            "200 OK",
            r#"{"id":"chatcmpl-2","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"the answer is 42"},"finish_reason":"stop"}]}"#,
        ),
    ]);
    let (mcp_url, mcp_thread) = start_mock_mcp_server();
    let config = ConfigDirectory::new(&format!("mcp_servers:\n  mock:\n    url: \"{mcp_url}\"\n",));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("the answer is 42"),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let first_body = without_json_whitespace(&first_request.body);
    assert!(
        first_body.contains(r#""name":"mock__echo""#),
        "first request body: {first_body}"
    );

    let second_body = without_json_whitespace(&second_request.body);
    assert!(
        second_body.contains(r#""role":"tool""#),
        "second request body: {second_body}"
    );
    assert!(
        second_body.contains(r#""tool_call_id":"call_1""#),
        "second request body: {second_body}"
    );
    assert!(
        second_body.contains("42"),
        "second request body should carry the tool's result: {second_body}"
    );
}

#[test]
fn rejects_a_repeated_tools_list_pagination_cursor() {
    let (mcp_url, mcp_thread) = start_mock_mcp_server_with_list_cursor(Some("same"));
    let config = ConfigDirectory::new(&format!("mcp_servers:\n  mock:\n    url: \"{mcp_url}\"\n",));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--mcp",
            "mock",
            "hello",
        ])
        .output()
        .expect("failed to execute lait");

    mcp_thread.join().expect("mock MCP server thread panicked");

    assert!(!output.status.success(), "repeated cursor should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("repeated a 'tools/list' pagination cursor 'same'"),
        "stderr: {stderr}"
    );
}

#[test]
fn retries_mcp_tool_listing_with_a_fresh_connection_after_a_failure() {
    let llm_server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let (mcp_url, mcp_thread) = start_recovering_mcp_server();
    let config = ConfigDirectory::new(&format!("mcp_servers:\n  mock:\n    url: \"{mcp_url}\"\n",));
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: test-model
nodes:
  call:
    prompt: "{{{{ input }}}}"
    mcp: [mock]
    retry:
      max_attempts: 2
steps:
  - use: call
"#,
        llm_server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .output()
        .expect("failed to execute lait run");
    let _ = llm_server.receive_request();
    llm_server.finish();
    let methods = mcp_thread
        .join()
        .expect("recovering MCP server thread panicked");

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| method.as_str() == "initialize")
            .count(),
        2,
        "MCP methods: {methods:?}"
    );
}
