mod support;

use support::{ConfigDirectory, MockServer, test_command, without_json_whitespace};

fn tool_call_response(tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"test-model","choices":[{{"index":0,"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"{tool}","arguments":"{arguments}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
    )
}

const FINAL_ANSWER_BODY: &str = r#"{"id":"chatcmpl-2","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;

#[test]
fn chat_mode_calls_a_shell_tool_and_returns_the_models_final_answer() {
    let llm_server = MockServer::start_sequence(&[
        (
            "200 OK",
            &tool_call_response("tool__echo", r#"{\"text\":\"hi there\"}"#),
        ),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let config =
        ConfigDirectory::new("tools:\n  echo:\n    command: [\"echo\", \"{{ input.text }}\"]\n");

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--tool",
            "echo",
            "what does echo say?",
        ])
        .output()
        .expect("failed to execute lait");

    let first_request = llm_server.receive_request();
    let second_request = llm_server.receive_request();
    llm_server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");

    let first_body = without_json_whitespace(&first_request.body);
    assert!(
        first_body.contains(r#""name":"tool__echo""#),
        "first request body: {first_body}"
    );

    let second_body = without_json_whitespace(&second_request.body);
    assert!(
        second_body.contains(r#""role":"tool""#) && second_body.contains("hithere"),
        "second request body should carry the command's output: {second_body}"
    );
}

#[test]
fn a_nonzero_exit_is_returned_as_a_tool_result_and_the_loop_continues() {
    let llm_server = MockServer::start_sequence(&[
        ("200 OK", &tool_call_response("tool__fail", r#"{}"#)),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let config =
        ConfigDirectory::new("tools:\n  fail:\n    command: [\"sh\", \"-c\", \"exit 3\"]\n");

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--tool",
            "fail",
            "try the failing tool",
        ])
        .output()
        .expect("failed to execute lait");

    let _first_request = llm_server.receive_request();
    let second_request = llm_server.receive_request();
    llm_server.finish();

    assert!(
        output.status.success(),
        "a failing shell tool should not fail the whole request: {output:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");

    let second_body = without_json_whitespace(&second_request.body);
    assert!(
        second_body.contains("toolcommandfailed"),
        "second request body should carry the failure text: {second_body}"
    );
}

#[test]
fn a_timeout_is_returned_as_a_tool_result_and_the_loop_continues() {
    let llm_server = MockServer::start_sequence(&[
        ("200 OK", &tool_call_response("tool__slow", r#"{}"#)),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let config = ConfigDirectory::new(
        "tools:\n  slow:\n    command: [\"sh\", \"-c\", \"sleep 5\"]\n    timeout: 1\n",
    );

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--tool",
            "slow",
            "try the slow tool",
        ])
        .output()
        .expect("failed to execute lait");

    let _first_request = llm_server.receive_request();
    let second_request = llm_server.receive_request();
    llm_server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let second_body = without_json_whitespace(&second_request.body);
    assert!(
        second_body.contains("timedout"),
        "second request body should carry the timeout text: {second_body}"
    );
}

#[test]
fn an_empty_command_list_is_a_lint_error() {
    // `lait lint` checks every 'tools:' entry in lait.config.yml regardless
    // of which files are named on the command line — so a trivial workflow
    // file is enough to trigger it; see `lint::check_shell_tool_definitions`.
    let config = ConfigDirectory::new("tools:\n  broken:\n    command: []\n");
    std::fs::write(
        config.path().join("wf.yml"),
        "nodes:\n  echo:\n    type: transform\n    jq: '.'\nsteps:\n  - use: echo\n",
    )
    .unwrap();

    let output = test_command()
        .current_dir(config.path())
        .args(["lint", "wf.yml"])
        .output()
        .expect("failed to execute lait lint");

    assert!(
        !output.status.success(),
        "lint should fail on an empty 'command' list: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("broken"), "stdout: {stdout}");
}

#[test]
fn tool_policy_deny_blocks_a_shell_tool_call_but_the_loop_still_reaches_a_final_answer() {
    let llm_server = MockServer::start_sequence(&[
        ("200 OK", &tool_call_response("tool__marker", r#"{}"#)),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let dir = ConfigDirectory::empty();
    let marker = dir.path().join("marker");
    let config = ConfigDirectory::new(&format!(
        "tools:\n  marker:\n    command: [\"touch\", \"{}\"]\ntool_policy:\n  deny: [\"tool__marker\"]\n",
        marker.display()
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--tool",
            "marker",
            "try the denied tool",
        ])
        .output()
        .expect("failed to execute lait");

    let _first_request = llm_server.receive_request();
    let second_request = llm_server.receive_request();
    llm_server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(
        !marker.exists(),
        "a denied tool call must never actually run its command"
    );
    let second_body = without_json_whitespace(&second_request.body);
    assert!(
        second_body.contains("tool_policy"),
        "second request body should carry the denial reason: {second_body}"
    );
}
