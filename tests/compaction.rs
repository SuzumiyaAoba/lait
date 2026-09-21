//! Behavioral coverage for `default.compaction:` (`src/config/types.rs`'s
//! `CompactionConfig`, driven from `engine/transport.rs`'s
//! `RequestSettings::maybe_compact`/`compact_tool_loop`): a tool loop
//! periodically summarizes its own growing history via an extra model call
//! instead of letting it grow unboundedly toward `max_tool_rounds`.

mod support;

use support::{ConfigDirectory, MockServer, test_command};

fn tool_call_response(arguments: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-tool","object":"chat.completion","created":0,"model":"workflow-model","choices":[{{"index":0,"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"tool__echo","arguments":"{arguments}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
    )
}

fn plain_response(content: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-plain","object":"chat.completion","created":0,"model":"workflow-model","choices":[{{"index":0,"message":{{"role":"assistant","content":"{content}"}},"finish_reason":"stop"}}]}}"#
    )
}

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
nodes:
  ask:
    type: prompt
    prompt: "{{{{ input }}}}"
    tools: [echo]
steps:
  - use: ask
"#
    )
}

/// Read every JSON line of a `--trace-file` as a `serde_json::Value`.
fn read_events(path: &std::path::Path) -> Vec<serde_json::Value> {
    let contents = std::fs::read_to_string(path).expect("trace file should exist");
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each trace line should be valid JSON"))
        .collect()
}

#[test]
fn a_tool_loop_compacts_periodically_and_still_reaches_a_final_answer() {
    // trigger_rounds: 2 means the loop compacts right before round 2 — one
    // extra "summarize so far" request lands between round 1's and round
    // 2's own requests. Request order: [round 1 (tool call), compaction
    // (summary), round 2 (tool call), round 3 (final answer)].
    let server = MockServer::start_sequence(&[
        ("200 OK", &tool_call_response(r#"{\"text\":\"r1\"}"#)),
        ("200 OK", &plain_response("summary of r1")),
        ("200 OK", &tool_call_response(r#"{\"text\":\"r2\"}"#)),
        ("200 OK", &plain_response("done")),
    ]);
    let config = ConfigDirectory::new(
        "tools:\n  echo:\n    command: [\"echo\", \"{{ input.text }}\"]\n\
         default:\n  compaction:\n    trigger_rounds: 2\n    keep_last_n: 2\n",
    );
    config.write("workflow.yml", &tools_workflow(&server.base_url));
    let trace_path = config.path().join("trace.jsonl");

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg("workflow.yml")
        .arg("start")
        .arg("--trace-file")
        .arg(&trace_path)
        .output()
        .expect("failed to execute lait run");

    for _ in 0..4 {
        server.receive_request();
    }
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");

    let events = read_events(&trace_path);
    let count_of = |operation: &str| {
        events
            .iter()
            .filter(|e| e["operation"] == operation)
            .count()
    };
    assert_eq!(count_of("chat"), 4, "events: {events:?}");
    assert_eq!(count_of("execute_tool"), 2, "events: {events:?}");
    assert_eq!(count_of("compact"), 1, "events: {events:?}");

    let compact_event = events
        .iter()
        .find(|e| e["operation"] == "compact")
        .expect("a compact event should be recorded");
    assert_eq!(compact_event["attributes"]["lait.compaction.round"], 2);
}

#[test]
fn compaction_is_off_by_default() {
    // Without `default.compaction:`, a run with the same shape never sends
    // the extra summarization request — only the 3 rounds themselves.
    let server = MockServer::start_sequence(&[
        ("200 OK", &tool_call_response(r#"{\"text\":\"r1\"}"#)),
        ("200 OK", &tool_call_response(r#"{\"text\":\"r2\"}"#)),
        ("200 OK", &plain_response("done")),
    ]);
    let config =
        ConfigDirectory::new("tools:\n  echo:\n    command: [\"echo\", \"{{ input.text }}\"]\n");
    config.write("workflow.yml", &tools_workflow(&server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg("workflow.yml")
        .arg("start")
        .output()
        .expect("failed to execute lait run");

    for _ in 0..3 {
        server.receive_request();
    }
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");
}

#[test]
fn a_zero_trigger_rounds_is_a_clear_configuration_error() {
    let server = MockServer::start("200 OK", &plain_response("unused"));
    // `tools:` must exist for the node's own `tools: [echo]` to resolve
    // (checked while assembling the tool loop, before the round loop — and
    // its `trigger_rounds` check — ever starts), even though this run never
    // actually reaches a tool call: it fails at the `trigger_rounds` check
    // on round 1, before any request is sent at all.
    let config = ConfigDirectory::new(
        "default:\n  compaction:\n    trigger_rounds: 0\ntools:\n  echo:\n    command: [\"echo\"]\n",
    );
    config.write("workflow.yml", &tools_workflow(&server.base_url));

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg("workflow.yml")
        .arg("start")
        .output()
        .expect("failed to execute lait run");

    // No request is ever expected: the trigger_rounds check runs before the
    // first round's request is sent, and errors before any bytes go out.
    let leaked = server.try_receive_request(std::time::Duration::from_millis(300));

    assert!(
        !output.status.success(),
        "a zero trigger_rounds should fail the run: {output:?}"
    );
    assert!(leaked.is_none(), "no request should ever be sent");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("trigger_rounds"), "{stderr}");
}
