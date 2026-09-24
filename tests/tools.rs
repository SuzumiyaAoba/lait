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
fn a_shell_tool_rejects_non_object_arguments_without_running_the_command() {
    let llm_server = MockServer::start("200 OK", &tool_call_response("tool__marker", r#"[]"#));
    let config =
        ConfigDirectory::new("tools:\n  marker:\n    command: [\"touch\", \"marker.txt\"]\n");

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--tool",
            "marker",
            "invalid arguments",
        ])
        .output()
        .expect("failed to execute lait");
    let _request = llm_server.receive_request();
    llm_server.finish();

    assert!(
        !output.status.success(),
        "expected invalid arguments to fail"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("JSON object"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !config.path().join("marker.txt").exists(),
        "a malformed tool call must not run the configured command"
    );
}

#[test]
fn an_empty_command_list_is_a_lint_error() {
    // `lait lint` checks every 'tools:' entry in lait.config.yml regardless
    // of which files are named on the command line — so a trivial workflow
    // file is enough to trigger it; see `lint::check_shell_tool_definitions`.
    let config = ConfigDirectory::new("tools:\n  broken:\n    command: []\n");
    config.write("wf.yml", "steps:\n  - jq: '.'\n");

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

#[test]
fn a_tool_with_an_env_allowlist_and_cwd_only_sees_what_it_was_given() {
    let llm_server = MockServer::start_sequence(&[
        ("200 OK", &tool_call_response("tool__probe", "{}")),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let path = std::env::var("PATH").unwrap_or_default();
    let config = ConfigDirectory::new(&format!(
        "tools:\n  probe:\n    command: [\"sh\", \"-c\", \"printf 'FOO=[%s] cwd=[%s]' \\\"$FOO\\\" \\\"$(pwd)\\\"\"]\n    env:\n      PATH: \"{path}\"\n      FOO: \"bar\"\n    cwd: \"{cwd}\"\n",
        cwd = config_scratch_cwd().display(),
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--tool",
            "probe",
            "run the probe",
        ])
        .output()
        .expect("failed to execute lait");

    llm_server.receive_request();
    let second_request = llm_server.receive_request();
    llm_server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");

    let second_body = without_json_whitespace(&second_request.body);
    assert!(second_body.contains("FOO=[bar]"), "{second_body}");
    let expected_cwd = std::fs::canonicalize(config_scratch_cwd())
        .unwrap()
        .display()
        .to_string();
    assert!(
        second_body.contains(&format!("cwd=[{expected_cwd}]")),
        "second request body should show the pinned cwd: {second_body}"
    );
}

/// A stable scratch directory (the system temp directory itself) to pin
/// `cwd:` to — this test only needs *some* directory that isn't the
/// invocation's own cwd, not a freshly created one.
fn config_scratch_cwd() -> std::path::PathBuf {
    std::env::temp_dir()
}
