mod support;

use support::{MockServer, ScratchDir, test_command, workflow_yaml};

fn model_config(base_url: &str) -> String {
    format!(
        "models:\n  m:\n    - provider:\n        base_url: \"{base_url}\"\n      model_id: model-a\n"
    )
}

fn completion_body(content: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":{content}}},"finish_reason":"stop"}}]}}"#
    )
}

#[test]
fn prompt_target_passes_contains_and_jq_assertions() {
    let server = MockServer::start("200 OK", &completion_body(r#""これは結論です""#));
    let scratch = ScratchDir::new();
    scratch.write("lait.config.yml", &model_config(&server.base_url));
    scratch.write(
        "eval.yml",
        r#"
target:
  model: m
  prompt: "Summarize: {{ input }}"
cases:
  - input: "some text"
    assert:
      - type: contains
        value: "結論"
      - type: jq
        expr: 'contains("結論")'
"#,
    );

    let output = test_command()
        .current_dir(scratch.path())
        .arg("eval")
        .arg("eval.yml")
        .output()
        .expect("failed to execute lait eval");

    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait eval failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1/1"), "{stdout}");
    assert!(stdout.contains("PASS"), "{stdout}");
    assert!(stdout.contains("1 of 1 case(s) fully passed"), "{stdout}");
}

#[test]
fn workflow_target_runs_the_workflow_and_evaluates_its_output() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"これは結論です"},"finish_reason":"stop"}]}"#,
    );
    let scratch = ScratchDir::new();
    scratch.write("workflow.yml", &workflow_yaml(&server.base_url));
    scratch.write(
        "eval.yml",
        r#"
target:
  workflow: ./workflow.yml
cases:
  - input: "hello"
    assert:
      - type: equals
        value: "これは結論です"
"#,
    );

    let output = test_command()
        .current_dir(scratch.path())
        .arg("eval")
        .arg("eval.yml")
        .output()
        .expect("failed to execute lait eval");

    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait eval failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PASS"), "{stdout}");
}

#[test]
fn llm_judge_assertion_passes_at_or_above_the_threshold() {
    // Two connections in order: the target's own completion, then the judge
    // model's structured-output score.
    let server = MockServer::start_sequence(&[
        ("200 OK", &completion_body(r#""a fine summary""#)),
        (
            "200 OK",
            &completion_body(r#""{\"score\": 0.9, \"reasoning\": \"good\"}""#),
        ),
    ]);
    let scratch = ScratchDir::new();
    scratch.write("lait.config.yml", &model_config(&server.base_url));
    scratch.write(
        "eval.yml",
        r#"
target:
  model: m
  prompt: "Summarize: {{ input }}"
cases:
  - input: "some text"
    assert:
      - type: llm_judge
        criteria: "is it a good summary?"
        model: m
        threshold: 0.7
"#,
    );

    let output = test_command()
        .current_dir(scratch.path())
        .arg("eval")
        .arg("eval.yml")
        .output()
        .expect("failed to execute lait eval");

    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait eval failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PASS"), "{stdout}");
}

#[test]
fn llm_judge_assertion_fails_below_the_threshold() {
    let server = MockServer::start_sequence(&[
        ("200 OK", &completion_body(r#""an incomplete summary""#)),
        (
            "200 OK",
            &completion_body(r#""{\"score\": 0.3, \"reasoning\": \"missing key points\"}""#),
        ),
    ]);
    let scratch = ScratchDir::new();
    scratch.write("lait.config.yml", &model_config(&server.base_url));
    scratch.write(
        "eval.yml",
        r#"
target:
  model: m
  prompt: "Summarize: {{ input }}"
cases:
  - input: "some text"
    assert:
      - type: llm_judge
        criteria: "is it a good summary?"
        model: m
        threshold: 0.7
"#,
    );

    let output = test_command()
        .current_dir(scratch.path())
        .arg("eval")
        .arg("eval.yml")
        .output()
        .expect("failed to execute lait eval");

    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "lait eval should exit non-zero when a case does not fully pass: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains("below threshold"), "{stdout}");
}

#[test]
fn repeat_aggregates_a_success_rate_and_fails_the_whole_run_on_a_partial_pass() {
    // Two repeats, one case: the first run's content satisfies the
    // `contains` assertion, the second run's doesn't — a 1/2 success rate.
    let server = MockServer::start_sequence(&[
        ("200 OK", &completion_body(r#""これは結論です""#)),
        ("200 OK", &completion_body(r#""まだ途中です""#)),
    ]);
    let scratch = ScratchDir::new();
    scratch.write("lait.config.yml", &model_config(&server.base_url));
    scratch.write(
        "eval.yml",
        r#"
target:
  model: m
  prompt: "Summarize: {{ input }}"
cases:
  - input: "some text"
    assert:
      - type: contains
        value: "結論"
"#,
    );

    let output = test_command()
        .current_dir(scratch.path())
        .arg("eval")
        .arg("--repeat")
        .arg("2")
        .arg("eval.yml")
        .output()
        .expect("failed to execute lait eval");

    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "a partial success rate should exit non-zero: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1/2"), "{stdout}");
    assert!(stdout.contains("FAIL"), "{stdout}");
}

#[test]
fn json_format_reports_per_case_success_rate_and_runs() {
    let server = MockServer::start("200 OK", &completion_body(r#""これは結論です""#));
    let scratch = ScratchDir::new();
    scratch.write("lait.config.yml", &model_config(&server.base_url));
    scratch.write(
        "eval.yml",
        r#"
target:
  model: m
  prompt: "Summarize: {{ input }}"
cases:
  - input: "some text"
    assert:
      - type: contains
        value: "結論"
"#,
    );

    let output = test_command()
        .current_dir(scratch.path())
        .arg("eval")
        .arg("--format")
        .arg("json")
        .arg("eval.yml")
        .output()
        .expect("failed to execute lait eval");

    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait eval failed: {output:?}");
    let results: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--format json output should be valid JSON");
    let results = results.as_array().expect("results should be a JSON array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["case"], 1);
    assert_eq!(results[0]["passed"], 1);
    assert_eq!(results[0]["total"], 1);
    assert_eq!(results[0]["success_rate"], 1.0);
    let runs = results[0]["runs"]
        .as_array()
        .expect("runs should be a JSON array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["passed"], true);
}

#[test]
fn usage_assertion_is_isolated_per_concurrent_case() {
    // Two cases run concurrently against the same target; each response
    // reports a different token count. If a case's `TrajectoryContext` ever
    // saw another case's usage mixed into its own (the bug `eval::run_case`
    // giving each run its own `RunContext` fixes), the contaminated case's
    // total would exceed 150 (15 + 150 = 165) and fail its own bound —
    // regardless of which case's request the mock server happens to answer
    // first, so this doesn't depend on request ordering.
    let server = MockServer::start_sequence(&[
        (
            "200 OK",
            r#"{"id":"c1","object":"chat.completion","created":0,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"small"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        ),
        (
            "200 OK",
            r#"{"id":"c2","object":"chat.completion","created":0,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"large"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150}}"#,
        ),
    ]);
    let scratch = ScratchDir::new();
    scratch.write("lait.config.yml", &model_config(&server.base_url));
    scratch.write(
        "eval.yml",
        r#"
target:
  model: m
  prompt: "Summarize: {{ input }}"
cases:
  - input: "case one"
    assert:
      - type: usage
        max_total_tokens: 150
  - input: "case two"
    assert:
      - type: usage
        max_total_tokens: 150
"#,
    );

    let output = test_command()
        .current_dir(scratch.path())
        .arg("eval")
        .arg("eval.yml")
        .output()
        .expect("failed to execute lait eval");

    server.receive_request();
    server.receive_request();
    server.finish();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "lait eval failed: {output:?}\nstdout: {stdout}"
    );
    assert!(stdout.contains("2 of 2 case(s) fully passed"), "{stdout}");
}

fn tool_call_response(tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"workflow-model","choices":[{{"index":0,"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"{tool}","arguments":"{arguments}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
    )
}

const FINAL_ANSWER_BODY: &str = r#"{"id":"chatcmpl-2","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;

#[test]
fn tool_called_and_step_output_assertions_pass_for_a_workflow_target() {
    let server = MockServer::start_sequence(&[
        (
            "200 OK",
            &tool_call_response("tool__echo", r#"{\"text\":\"hi\"}"#),
        ),
        ("200 OK", FINAL_ANSWER_BODY),
    ]);
    let scratch = ScratchDir::new();
    scratch.write(
        "lait.config.yml",
        "tools:\n  echo:\n    command: [\"echo\", \"{{ input.text }}\"]\n",
    );
    scratch.write(
        "workflow.yml",
        &format!(
            r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
steps:
  - id: ask
    prompt: "{{{{ input }}}}"
    tools: [echo]
"#,
            server.base_url
        ),
    );
    scratch.write(
        "eval.yml",
        r#"
target:
  workflow: ./workflow.yml
cases:
  - input: "what does echo say?"
    assert:
      - type: equals
        value: "done"
      - type: tool_called
        name: tool__echo
        min: 1
        max: 1
        args_jq: '.text == "hi"'
      - type: step_output
        id: ask
        assert:
          - type: equals
            value: "done"
"#,
    );

    let output = test_command()
        .current_dir(scratch.path())
        .arg("eval")
        .arg("eval.yml")
        .output()
        .expect("failed to execute lait eval");

    server.receive_request();
    server.receive_request();
    server.finish();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "lait eval failed: {output:?}\nstdout: {stdout}"
    );
    assert!(stdout.contains("1 of 1 case(s) fully passed"), "{stdout}");
}
