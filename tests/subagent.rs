mod support;

use support::{
    AgentMarkdownFile, ConfigDirectory, MockServer, test_command, without_json_whitespace,
};

/// Chat mode, with a subagent registered in `lait.config.yml`'s `agents:`
/// and made available via `--subagent`, calls it as a tool. Both the
/// top-level request and the subagent's own (recursive) completion go
/// through the same mock LLM server — first the top-level model returns a
/// `tool_calls` response naming the subagent tool, then the subagent's own
/// completion request gets its final answer, then the top-level model gets
/// the tool result and returns its own final answer.
#[test]
fn chat_mode_calls_a_subagent_tool_and_returns_the_models_final_answer() {
    let llm_server = MockServer::start_sequence(&[
        (
            "200 OK",
            r#"{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"agent__researcher","arguments":"{\"input\":\"look into it\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ),
        (
            "200 OK",
            r#"{"id":"chatcmpl-2","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"research findings: 42"},"finish_reason":"stop"}]}"#,
        ),
        (
            "200 OK",
            r#"{"id":"chatcmpl-3","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"the answer is 42"},"finish_reason":"stop"}]}"#,
        ),
    ]);

    // No `input_schema`: the tool call's `{"input": "..."}` argument is
    // unwrapped down to that bare value before it reaches this template (see
    // `app::subagent_tool_input`), the same way a plain-text `INPUT` argument
    // to `lait agent run` becomes `{{ input }}` directly.
    let subagent = AgentMarkdownFile::new(
        "---\nname: researcher\ndescription: looks into a task\nmodel: test-model\n---\nResearch: {{ input }}\n",
    );
    // A subagent's own `RequestSettings` are resolved independently of the
    // top-level chat invocation's `--base-url` (that CLI flag only applies to
    // the top-level request) — see `call_subagent_tool`. Setting `base_url:`
    // here is what routes the subagent's own completion to the same mock
    // server.
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\nagents:\n  researcher: \"{}\"\n",
        llm_server.base_url,
        subagent.path.display()
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            &llm_server.base_url,
            "--subagent",
            "researcher",
            "what is the answer?",
        ])
        .output()
        .expect("failed to execute lait");

    let first_request = llm_server.receive_request();
    let second_request = llm_server.receive_request();
    let third_request = llm_server.receive_request();
    llm_server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("the answer is 42"),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let first_body = without_json_whitespace(&first_request.body);
    assert!(
        first_body.contains(r#""name":"agent__researcher""#),
        "first request body: {first_body}"
    );

    assert!(
        second_request.body.contains("look into it"),
        "the subagent's own request should carry the tool call's input: {}",
        second_request.body
    );

    let third_body = without_json_whitespace(&third_request.body);
    assert!(
        third_body.contains(r#""role":"tool""#),
        "third request body: {third_body}"
    );
    assert!(
        third_body.contains(r#""tool_call_id":"call_1""#),
        "third request body: {third_body}"
    );
    assert!(
        third_request.body.contains("research findings"),
        "third request body should carry the subagent's result: {}",
        third_request.body
    );
}

/// A `subagents:` name with nothing registered under `agents:` in
/// `lait.config.yml` fails clearly instead of silently doing nothing.
#[test]
fn chat_mode_fails_clearly_on_an_unknown_subagent_name() {
    let config = ConfigDirectory::new("");

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "test-model",
            "--base-url",
            "http://127.0.0.1:1",
            "--subagent",
            "nope",
            "hello",
        ])
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown subagent 'nope'"),
        "stderr: {stderr}"
    );
}
