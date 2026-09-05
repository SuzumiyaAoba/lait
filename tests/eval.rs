mod support;

use support::{MockServer, ScratchDir, test_command};

fn model_config(base_url: &str) -> String {
    format!(
        "models:\n  m:\n    - provider:\n        base_url: \"{base_url}\"\n      model_id: model-a\n"
    )
}

fn workflow_yaml(base_url: &str) -> String {
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
  call:
    type: prompt
    prompt: "{{{{ input }}}}"
steps:
  - use: call
"#
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
