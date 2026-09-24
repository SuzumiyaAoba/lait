//! Behavioral coverage for `lait run --trace-file` (`src/trace.rs`'s
//! `TraceCollector`/`write_jsonl`, wired through `engine::transport`/
//! `engine::tool_loop`) and `lait trace show`.

mod support;

use support::{ConfigDirectory, MockServer, ScratchDir, WorkflowFile, test_command};

const RESPONSE_WITH_USAGE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":22,"total_tokens":33}}"#;

/// One line of a `--trace-file` JSONL log, parsed as JSON.
fn read_events(path: &std::path::Path) -> Vec<serde_json::Value> {
    let contents = std::fs::read_to_string(path).expect("trace file should exist");
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each trace line should be valid JSON"))
        .collect()
}

#[test]
fn a_workflow_prompt_step_records_a_chat_event_with_usage() {
    let server = MockServer::start("200 OK", RESPONSE_WITH_USAGE);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
steps:
  - id: greet
    prompt: "{{{{ input }}}}"
"#,
        server.base_url
    ));
    let scratch = ScratchDir::new();
    let trace_path = scratch.path().join("trace.jsonl");

    let output = test_command()
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .arg("--trace-file")
        .arg(&trace_path)
        .output()
        .expect("failed to execute lait run");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");

    let events = read_events(&trace_path);
    assert_eq!(
        events.len(),
        1,
        "expected exactly one chat event: {events:?}"
    );
    let event = &events[0];
    assert_eq!(event["operation"], "chat");
    assert_eq!(event["label"], "greet");
    assert_eq!(event["attributes"]["lait.source"], "live");
    assert_eq!(
        event["attributes"]["gen_ai.request.model"],
        "workflow-model"
    );
    assert_eq!(event["attributes"]["gen_ai.usage.input_tokens"], 11);
    assert_eq!(event["attributes"]["gen_ai.usage.output_tokens"], 22);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("trace written to") && stderr.contains("1 event"),
        "stderr should note the trace write: {stderr}"
    );
}

#[test]
fn lait_trace_show_prints_recorded_events() {
    let server = MockServer::start("200 OK", RESPONSE_WITH_USAGE);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
steps:
  - id: greet
    prompt: "{{{{ input }}}}"
"#,
        server.base_url
    ));
    let scratch = ScratchDir::new();
    let trace_path = scratch.path().join("trace.jsonl");

    let run_output = test_command()
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .arg("--trace-file")
        .arg(&trace_path)
        .output()
        .expect("failed to execute lait run");
    server.receive_request();
    server.finish();
    assert!(
        run_output.status.success(),
        "lait run failed: {run_output:?}"
    );

    let show_output = test_command()
        .arg("trace")
        .arg("show")
        .arg(&trace_path)
        .output()
        .expect("failed to execute lait trace show");
    assert!(
        show_output.status.success(),
        "lait trace show failed: {show_output:?}"
    );
    let stdout = String::from_utf8_lossy(&show_output.stdout);
    assert!(
        stdout.contains("chat"),
        "stdout should list the chat event: {stdout}"
    );
    assert!(
        stdout.contains("greet"),
        "stdout should carry the step label: {stdout}"
    );
}

fn tool_call_response(tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"workflow-model","choices":[{{"index":0,"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"{tool}","arguments":"{arguments}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
    )
}

const FINAL_ANSWER_BODY: &str = r#"{"id":"chatcmpl-2","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;

fn tools_workflow(base_url: &str) -> String {
    format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{base_url}"
      model_id: workflow-model
steps:
  - id: ask-step
    prompt: "{{{{ input }}}}"
    tools: [echo]
"#
    )
}

#[test]
fn an_allowed_tool_call_records_an_execute_tool_event() {
    let server = MockServer::start_sequence(&[
        (
            "200 OK",
            &tool_call_response("tool__echo", r#"{\"text\":\"hi\"}"#),
        ),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let config =
        ConfigDirectory::new("tools:\n  echo:\n    command: [\"echo\", \"{{ input.text }}\"]\n");
    let workflow = WorkflowFile::new(&tools_workflow(&server.base_url));
    let scratch = ScratchDir::new();
    let trace_path = scratch.path().join("trace.jsonl");

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow.path)
        .arg("what does echo say?")
        .arg("--trace-file")
        .arg(&trace_path)
        .output()
        .expect("failed to execute lait run");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");

    let events = read_events(&trace_path);
    let chat_events: Vec<_> = events
        .iter()
        .filter(|event| event["operation"] == "chat")
        .collect();
    assert_eq!(
        chat_events.len(),
        2,
        "expected one chat event per round: {events:?}"
    );

    let tool_event = events
        .iter()
        .find(|event| event["operation"] == "execute_tool")
        .unwrap_or_else(|| panic!("expected an execute_tool event: {events:?}"));
    assert_eq!(tool_event["label"], "tool 'tool__echo'");
    assert_eq!(tool_event["attributes"]["gen_ai.tool.name"], "tool__echo");
    assert_eq!(tool_event["attributes"]["lait.tool.decision"], "allowed");
    assert_eq!(tool_event["attributes"]["lait.tool.round"], 1);
}

#[test]
fn a_tool_policy_denial_records_an_execute_tool_event_with_the_denial_reason() {
    let server = MockServer::start_sequence(&[
        (
            "200 OK",
            &tool_call_response("tool__echo", r#"{\"text\":\"hi\"}"#),
        ),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let config = ConfigDirectory::new(
        "tools:\n  echo:\n    command: [\"echo\", \"{{ input.text }}\"]\ntool_policy:\n  deny: [\"tool__echo\"]\n",
    );
    let workflow = WorkflowFile::new(&tools_workflow(&server.base_url));
    let scratch = ScratchDir::new();
    let trace_path = scratch.path().join("trace.jsonl");

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow.path)
        .arg("what does echo say?")
        .arg("--trace-file")
        .arg(&trace_path)
        .output()
        .expect("failed to execute lait run");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");

    let events = read_events(&trace_path);
    let tool_event = events
        .iter()
        .find(|event| event["operation"] == "execute_tool")
        .unwrap_or_else(|| panic!("expected an execute_tool event: {events:?}"));
    assert_eq!(tool_event["attributes"]["lait.tool.decision"], "denied");
    assert!(
        tool_event["attributes"]["lait.tool.denial_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("tool_policy"),
        "denial reason should mention tool_policy: {tool_event}"
    );
}
