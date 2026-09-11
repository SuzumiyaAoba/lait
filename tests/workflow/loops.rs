use super::*;

#[test]
fn loop_while_reruns_steps_until_the_pre_check_condition_goes_false() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  bump:
    type: transform
    jq: '{n: (.n + 1)}'
steps:
  - id: count-up
    loop:
      while: '.n < 3'
      max_iterations: 10
      steps:
        - use: bump
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        r#"{"n":3}"#
    );
}

#[test]
fn loop_while_runs_zero_times_when_the_condition_starts_false() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  never:
    type: transform
    jq: '"should not run"'
steps:
  - loop:
      while: '.n < 0'
      max_iterations: 10
      steps:
        - use: never
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":5}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        r#"{"n":5}"#
    );
}

#[test]
fn loop_while_fails_when_max_iterations_is_reached_without_the_condition_going_false() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    type: transform
    jq: '.'
steps:
  - loop:
      while: 'true'
      max_iterations: 3
      steps:
        - use: passthrough
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(
        !output.status.success(),
        "expected an unsatisfied while-loop to fail once max_iterations is reached"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("max_iterations"), "stderr: {stderr}");
}

#[test]
fn loop_until_runs_at_least_once_and_stops_once_the_post_check_condition_is_true() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  bump:
    type: transform
    jq: '{n: (.n + 1)}'
steps:
  - id: retry
    loop:
      until: '.n >= 3'
      max_iterations: 10
      steps:
        - use: bump
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        r#"{"n":3}"#
    );
}

#[test]
fn loop_until_fails_when_max_iterations_is_reached_without_the_condition_going_true() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    type: transform
    jq: '.'
steps:
  - loop:
      until: 'false'
      max_iterations: 3
      steps:
        - use: passthrough
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(
        !output.status.success(),
        "expected an unsatisfied until-loop to fail once max_iterations is reached"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("max_iterations"), "stderr: {stderr}");
}

#[test]
fn for_each_passes_a_string_item_to_a_prompt_unquoted() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  summarize:
    type: prompt
    prompt: "summarize: {{{{ input }}}}"
steps:
  - for_each:
      items: '.names'
      steps:
        - use: summarize
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, r#"{"names":["Alice"]}"#);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""content":"summarize:Alice""#),
        "expected the string item to be passed through unquoted, request body: {body}"
    );
}

#[test]
fn for_each_runs_steps_per_item_and_collects_results_in_order() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  doubler:
    type: transform
    jq: '. * 2'
steps:
  - id: double
    for_each:
      items: '.items'
      steps:
        - use: doubler
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[1,2,3]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        "[2,4,6]"
    );
}

#[test]
fn for_each_join_filter_combines_the_collected_array_into_the_next_input() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  doubler:
    type: transform
    jq: '. * 2'
steps:
  - for_each:
      items: '.items'
      steps:
        - use: doubler
      join: 'add'
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[1,2,3]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "12");
}

#[test]
fn for_each_runs_zero_times_on_an_empty_array_and_yields_an_empty_result() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  never:
    type: transform
    jq: '"should not run"'
steps:
  - for_each:
      items: '.items'
      steps:
        - use: never
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[]");
}

#[test]
fn for_each_fails_when_items_does_not_produce_a_json_array() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    type: transform
    jq: '.'
steps:
  - for_each:
      items: '.count'
      steps:
        - use: passthrough
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"count":3}"#);

    assert!(
        !output.status.success(),
        "expected a non-array 'items' output to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must produce a JSON array"),
        "stderr: {stderr}"
    );
}
