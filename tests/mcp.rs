mod support;

use std::{
    io::Write,
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::{fs, path::Path};

use support::{
    ConfigDirectory, HttpRequest, MockServer, WorkflowFile, read_request, test_command,
    without_json_whitespace,
};

#[cfg(unix)]
use support::next_temp_path;

const CHAT_COMPLETION_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#;

#[cfg(unix)]
const STDIO_MCP_BLOCKING_TOOL_SCRIPT: &str = r#"#!/bin/sh
set -eu
(
  sleep 3
  printf alive > "$ALIVE"
) &
printf '%s' "$!" > "$MARKER"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"stdio","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echoes input","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*)
      sleep 30
      ;;
  esac
done
"#;

#[cfg(unix)]
const STDIO_MCP_BLOCKING_INITIALIZE_SCRIPT: &str = r#"#!/bin/sh
set -eu
(
  sleep 3
  printf alive > "$ALIVE"
) &
printf '%s' "$!" > "$MARKER"
sleep 30
"#;

#[cfg(unix)]
fn write_stdio_script(contents: &str) -> std::path::PathBuf {
    let path = next_temp_path("lait-test-mcp-stdio", ".sh");
    fs::write(&path, contents).expect("failed to write stdio MCP script");
    path
}

#[cfg(unix)]
fn stdio_mcp_config(script: &Path, marker: &Path, alive: &Path) -> ConfigDirectory {
    ConfigDirectory::new(&format!(
        "mcp_servers:\n  mock:\n    command: sh\n    args: [\"{}\"]\n    env:\n      MARKER: \"{}\"\n      ALIVE: \"{}\"\n",
        script.display(),
        marker.display(),
        alive.display(),
    ))
}

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

/// Starts a bounded OpenAI-compatible server for timeout/retry tests. Unlike
/// `MockServer::start_sequence`, this helper returns after a short deadline
/// when a buggy client never reaches the final request, so a stale MCP
/// connection cannot leave the test fixture blocked forever.
fn start_sequence_llm_server(
    responses: &[&'static str],
) -> (String, std::thread::JoinHandle<Vec<HttpRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock LLM server");
    listener
        .set_nonblocking(true)
        .expect("failed to make mock LLM server non-blocking");
    let addr = listener
        .local_addr()
        .expect("failed to read mock LLM server address");
    let responses: Vec<String> = responses
        .iter()
        .map(|response| (*response).to_owned())
        .collect();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut requests = Vec::new();
        while requests.len() < responses.len() && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("failed to accept mock LLM request: {error}"),
            };
            stream
                .set_nonblocking(false)
                .expect("failed to make accepted mock LLM connection blocking");
            let request = read_request(&mut stream).expect("failed to read mock LLM request");
            let response_body = &responses[requests.len()];
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len(),
            );
            stream
                .write_all(response.as_bytes())
                .expect("failed to write mock LLM response");
            stream.flush().expect("failed to flush mock LLM response");
            requests.push(request);
        }
        requests
    });
    (format!("http://{addr}/v1"), handle)
}

/// Starts an MCP server whose first `tools/call` is intentionally left
/// unanswered. A workflow timeout must evict that service before its retry;
/// the second round therefore has to perform a fresh handshake and receives
/// a normal tool result. Each HTTP request is handled on its own thread so the
/// unanswered first call does not prevent the listener from accepting retry
/// traffic.
fn start_timeout_retry_mcp_server() -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock MCP server");
    listener
        .set_nonblocking(true)
        .expect("failed to make mock MCP server non-blocking");
    let addr = listener
        .local_addr()
        .expect("failed to read mock MCP server address");
    let methods = Arc::new(Mutex::new(Vec::new()));
    let call_count = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn({
        let methods = Arc::clone(&methods);
        let call_count = Arc::clone(&call_count);
        move || {
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut workers = Vec::new();
            while Instant::now() < deadline && methods.lock().unwrap().len() < 8 {
                let (stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("failed to accept mock MCP request: {error}"),
                };
                let methods = Arc::clone(&methods);
                let call_count = Arc::clone(&call_count);
                workers.push(std::thread::spawn(move || {
                    let mut stream = stream;
                    stream
                        .set_nonblocking(false)
                        .expect("failed to make accepted mock MCP connection blocking");
                    let request = read_request(&mut stream).expect("failed to read MCP request");
                    let request: serde_json::Value = serde_json::from_str(&request.body)
                        .expect("mock MCP server got non-JSON body");
                    let method = request
                        .get("method")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let id = request.get("id").cloned();
                    methods.lock().unwrap().push(method.clone());

                    if method == "tools/call" {
                        let call_index = call_count.fetch_add(1, Ordering::AcqRel);
                        if call_index == 0 {
                            // Keep the first request in flight long enough for
                            // the one-second workflow timeout to fire. The
                            // client must not be able to reuse this service on
                            // its retry while it is still pending.
                            std::thread::sleep(Duration::from_secs(2));
                            return;
                        }
                    }

                    let (status, response_body) = match method.as_str() {
                        "initialize" => (
                            "200 OK",
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2025-06-18",
                                    "capabilities": {},
                                    "serverInfo": {"name": "timeout-retry-mcp", "version": "0.0.1"}
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
                                        "description": "returns a fresh result",
                                        "inputSchema": {"type": "object"}
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
                                    "content": [{"type": "text", "text": "fresh-result"}]
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
                        .expect("failed to write mock MCP response");
                    stream.flush().expect("failed to flush mock MCP response");
                }));
            }
            for worker in workers {
                worker.join().expect("mock MCP request worker panicked");
            }
            Arc::try_unwrap(methods)
                .expect("mock MCP methods still have outstanding references")
                .into_inner()
                .expect("mock MCP methods mutex was poisoned")
        }
    });
    (format!("http://{addr}/mcp"), handle)
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
            stream
                .set_nonblocking(false)
                .expect("failed to make accepted mock MCP connection blocking");
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

#[test]
fn a_timed_out_mcp_call_is_evicted_before_the_retry_uses_a_fresh_connection() {
    let (llm_url, llm_thread) = start_sequence_llm_server(&[
        r#"{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"mock__echo","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        r#"{"id":"chatcmpl-2","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_2","type":"function","function":{"name":"mock__echo","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        r#"{"id":"chatcmpl-3","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"retry succeeded"},"finish_reason":"stop"}]}"#,
    ]);
    let (mcp_url, mcp_thread) = start_timeout_retry_mcp_server();
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
    timeout: 1
    retry:
      max_attempts: 2
steps:
  - use: call
"#,
        llm_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .output()
        .expect("failed to execute lait run");
    let llm_requests = llm_thread.join().expect("mock LLM server thread panicked");
    let methods = mcp_thread.join().expect("mock MCP server thread panicked");

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "retry succeeded"
    );
    assert_eq!(llm_requests.len(), 3, "LLM requests: {llm_requests:?}");
    assert_eq!(
        methods
            .iter()
            .filter(|method| method.as_str() == "initialize")
            .count(),
        2,
        "the retry must initialize a fresh MCP connection: {methods:?}"
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| method.as_str() == "tools/call")
            .count(),
        2,
        "both attempts should reach tools/call: {methods:?}"
    );
}

#[cfg(unix)]
fn assert_stdio_descendant_was_stopped(marker: &Path, alive: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "the stdio MCP server did not start its descendant"
    );

    // The descendant is deliberately scheduled to create this file after the
    // timeout. A process-group cleanup that only kills the direct shell would
    // leave the marker behind, making the failure deterministic without
    // relying on a potentially-zombie PID and `kill -0`.
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !alive.exists(),
        "MCP descendant survived process-tree shutdown: {}",
        alive.display()
    );
}

#[cfg(unix)]
#[test]
fn timed_out_stdio_mcp_tool_call_stops_and_reaps_its_descendant() {
    let script = write_stdio_script(STDIO_MCP_BLOCKING_TOOL_SCRIPT);
    let marker = next_temp_path("lait-test-mcp-descendant", ".pid");
    let alive = next_temp_path("lait-test-mcp-descendant", ".alive");
    let config = stdio_mcp_config(&script, &marker, &alive);
    let (llm_url, llm_thread) = start_sequence_llm_server(&[
        r#"{"id":"chatcmpl-stdio","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_stdio","type":"function","function":{"name":"mock__echo","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
    ]);
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
    timeout: 1
steps:
  - use: call
"#,
        llm_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .args(["run", workflow.path.to_str().unwrap(), "hello"])
        .output()
        .expect("failed to execute lait run");
    let llm_requests = llm_thread.join().expect("mock LLM server thread panicked");

    assert!(!output.status.success(), "a blocked MCP call must time out");
    assert_eq!(
        llm_requests.len(),
        1,
        "LLM requests: {llm_requests:?}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_stdio_descendant_was_stopped(&marker, &alive);

    let _ = fs::remove_file(script);
    let _ = fs::remove_file(marker);
    let _ = fs::remove_file(alive);
}

#[cfg(unix)]
#[test]
fn cancelled_stdio_mcp_initialization_stops_and_reaps_its_descendant() {
    let script = write_stdio_script(STDIO_MCP_BLOCKING_INITIALIZE_SCRIPT);
    let marker = next_temp_path("lait-test-mcp-init-descendant", ".pid");
    let alive = next_temp_path("lait-test-mcp-init-descendant", ".alive");
    let config = stdio_mcp_config(&script, &marker, &alive);
    let workflow = WorkflowFile::new(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "http://127.0.0.1:1/v1"
      model_id: test-model
nodes:
  call:
    prompt: "{{ input }}"
    mcp: [mock]
    timeout: 1
steps:
  - use: call
"#,
    );

    let output = test_command()
        .current_dir(config.path())
        .args(["run", workflow.path.to_str().unwrap(), "hello"])
        .output()
        .expect("failed to execute lait run");

    assert!(
        !output.status.success(),
        "an MCP handshake that never responds must be cancelled"
    );
    assert_stdio_descendant_was_stopped(&marker, &alive);

    let _ = fs::remove_file(script);
    let _ = fs::remove_file(marker);
    let _ = fs::remove_file(alive);
}
