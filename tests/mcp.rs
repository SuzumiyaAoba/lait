mod support;

use std::io::{Read, Write};
use std::net::TcpListener;

use support::{ConfigDirectory, MockServer, test_command, without_json_whitespace};

/// A hand-rolled streamable-HTTP MCP server for integration tests: routes on
/// the JSON-RPC `method` field (something `support::MockServer` can't do,
/// since it just replays canned bodies in connection order) and answers
/// `initialize` / `notifications/initialized` / `tools/list` / `tools/call` —
/// the four requests one tool-call round trip makes. Exposes a single tool,
/// `echo`, that always returns a fixed string regardless of its arguments.
fn start_mock_mcp_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock MCP server");
    let addr = listener
        .local_addr()
        .expect("failed to read mock MCP server address");
    let handle = std::thread::spawn(move || {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().expect("failed to accept connection");
            let mut buf = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 4096];
                let read = stream.read(&mut chunk).expect("failed to read request");
                assert!(read > 0, "connection closed before headers were complete");
                buf.extend_from_slice(&chunk[..read]);
                if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
            let content_length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while buf.len() < header_end + content_length {
                let mut chunk = [0u8; 4096];
                let read = stream
                    .read(&mut chunk)
                    .expect("failed to read request body");
                assert!(read > 0, "connection closed before body was complete");
                buf.extend_from_slice(&chunk[..read]);
            }
            let body =
                String::from_utf8_lossy(&buf[header_end..header_end + content_length]).into_owned();
            let request: serde_json::Value =
                serde_json::from_str(&body).expect("mock MCP server got non-JSON body");
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
