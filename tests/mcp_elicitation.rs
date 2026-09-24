//! Behavioral coverage for `mcp_servers.<name>.allow_elicitation`
//! (`src/mcp/elicitation.rs`'s `LaitClientHandler`): a server sends an
//! `elicitation/create` request mid-`tools/call`, and lait answers it before
//! the server's own tool result comes back. Unix-only, mirroring
//! `tests/mcp.rs`'s own stdio MCP fixtures (a hand-written `sh` script
//! speaking newline-delimited JSON-RPC over stdio).
//!
//! `test_command()`'s child process never has an interactive stdin (there is
//! no tty under `cargo test`), so every case here exercises the
//! non-interactive "decline without prompting" path — the same real,
//! wire-level request/response round trip a real terminal session would use,
//! just always answered the same way. `tests/ask.rs` covers `ask:` steps the
//! same way, for the same reason (see its own doc comment).

#![cfg(unix)]

mod support;

use std::fs;

use support::{ConfigDirectory, WorkflowFile, next_temp_path, test_command};

/// Sends a server-initiated `elicitation/create` (mode `"form"`, the
/// 2025-11-25+ wire shape) as soon as a `tools/call` arrives, waits for the
/// client's correlated reply, and echoes the reply's `action` field back as
/// the tool's own result text — so the test can assert on it without
/// parsing JSON in shell.
const STDIO_MCP_ELICITING_TOOL_SCRIPT: &str = r#"#!/bin/sh
set -eu
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"stdio","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"ask_name","description":"asks for the users name","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":"srv-1","method":"elicitation/create","params":{"mode":"form","message":"What is your name?","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}}\n'
      read -r reply
      action=$(printf '%s\n' "$reply" | sed -n 's/.*"action":"\([a-z]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"elicitation action: %s"}]}}\n' "$id" "$action"
      ;;
  esac
done
"#;

fn write_stdio_script(contents: &str) -> std::path::PathBuf {
    let path = next_temp_path("lait-test-mcp-elicit", ".sh");
    fs::write(&path, contents).expect("failed to write stdio MCP script");
    path
}

fn workflow_calling_the_mock_tool(base_url: &str) -> String {
    format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{base_url}"
      model_id: test-model
steps:
  - id: call
    prompt: "{{{{ input }}}}"
    mcp: [mock]
"#
    )
}

#[test]
fn a_server_eliciting_mid_tool_call_is_declined_without_an_interactive_terminal() {
    let script = write_stdio_script(STDIO_MCP_ELICITING_TOOL_SCRIPT);
    let config = ConfigDirectory::new(&format!(
        "mcp_servers:\n  mock:\n    command: sh\n    args: [\"{}\"]\n    allow_elicitation: true\n",
        script.display()
    ));
    let llm_server = support::MockServer::start_sequence(&[
        (
            "200 OK",
            r#"{"id":"chatcmpl-elicit","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_elicit","type":"function","function":{"name":"mock__ask_name","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ),
        ("200 OK", &support::completion_body("test-model", "done")),
    ]);
    let workflow = WorkflowFile::new(&workflow_calling_the_mock_tool(&llm_server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["run", workflow.path.to_str().unwrap(), "hello"])
        .output()
        .expect("failed to execute lait run");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");

    llm_server.receive_request();
    let second_request = llm_server.receive_request();
    llm_server.finish();
    let second_json: serde_json::Value =
        serde_json::from_str(&second_request.body).expect("request body should be valid JSON");
    let tool_result = second_json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("a tool result message should be present");
    assert_eq!(
        tool_result["content"].as_str(),
        Some("elicitation action: decline"),
        "the server should have observed a decline: {tool_result:?}"
    );

    let _ = fs::remove_file(script);
}

#[test]
fn elicitation_is_declined_when_allow_elicitation_is_not_set() {
    // Same script, no `allow_elicitation:` at all — the server still gets a
    // (protocol-valid) decline back, just via the config gate rather than
    // the missing-terminal one; externally the two are indistinguishable,
    // which is exactly the point of this test (the default is safe even
    // when this feature is never mentioned in the config at all).
    let script = write_stdio_script(STDIO_MCP_ELICITING_TOOL_SCRIPT);
    let config = ConfigDirectory::new(&format!(
        "mcp_servers:\n  mock:\n    command: sh\n    args: [\"{}\"]\n",
        script.display()
    ));
    let llm_server = support::MockServer::start_sequence(&[
        (
            "200 OK",
            r#"{"id":"chatcmpl-elicit","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_elicit","type":"function","function":{"name":"mock__ask_name","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ),
        ("200 OK", &support::completion_body("test-model", "done")),
    ]);
    let workflow = WorkflowFile::new(&workflow_calling_the_mock_tool(&llm_server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["run", workflow.path.to_str().unwrap(), "hello"])
        .output()
        .expect("failed to execute lait run");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");

    llm_server.receive_request();
    llm_server.receive_request();
    llm_server.finish();

    let _ = fs::remove_file(script);
}
