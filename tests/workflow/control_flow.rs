use super::*;

#[test]
fn stop_ends_the_workflow_with_the_current_steps_output_and_skips_later_steps() {
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
steps:
  - id: call
    prompt: "{{{{ input }}}}"
  - stop: true
  - id: never
    jq: '"should not run"'
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn switch_records_its_named_output_before_bubbling_break() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - until: 'true'
    max_iterations: 1
    steps:
      - id: route
        switch:
          - when: 'true'
            steps:
              - break: true
  - id: read_route
    jq: '$steps.route'
"#,
    );

    let output = run_lait_workflow(&workflow.path, "route input");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "route input",
        "the switch's named output should remain available after its branch breaks"
    );
}

#[test]
fn on_error_records_its_named_output_before_bubbling_break() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - until: 'true'
    max_iterations: 1
    steps:
      - id: failed
        run: ["sh", "-c", "exit 1"]
        on_error:
          - id: recover
            jq: '"recovered"'
          - break: true
  - id: read_failure
    jq: '$steps.failed'
"#,
    );

    let output = run_lait_workflow(&workflow.path, "input");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "recovered",
        "the failed step's on_error output should remain available after it breaks"
    );
}

#[test]
fn break_stops_a_loop_before_its_until_condition_or_max_iterations() {
    let workflow = WorkflowFile::new(
        r#"
input_schema: {type: object}
steps:
  - until: '.n >= 10'
    max_iterations: 5
    steps:
      - id: bump
        jq: '.n += 1'
      - when: '.n == 2'
        break: true
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), r#"{"n":2}"#);
}

#[test]
fn break_stops_a_for_each_early_and_joins_only_the_items_processed_so_far() {
    let workflow = WorkflowFile::new(
        r#"
input_schema: {type: object}
steps:
  - for_each: '.items'
    steps:
      - when: '. == 2'
        break: true
      - id: passthrough
        jq: '.'
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[1,2,3]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[1,2]");
}

#[test]
fn stop_inside_a_loop_ends_the_whole_workflow_not_just_the_loop() {
    let workflow = WorkflowFile::new(
        r#"
input_schema: {type: object}
steps:
  - until: '.n >= 10'
    max_iterations: 5
    steps:
      - id: bump
        jq: '.n += 1'
      - when: '.n == 2'
        stop: true
  - id: never
    jq: '"should not run"'
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), r#"{"n":2}"#);
}

#[test]
fn the_top_level_output_shapes_the_result_even_after_stop() {
    let workflow = WorkflowFile::new(
        r#"
inputs:
  suffix: { type: string, default: "!" }
output: '{value: ., first: $steps.first, suffix: $inputs.suffix}'
steps:
  - id: first
    jq: 'ascii_upcase'
  - when: 'startswith("STOP")'
    output: '. + " (stopped)"'
    stop: true
  - jq: '"unreachable"'
"#,
    );

    let output = run_workflow_with(&workflow, &["stop now"]);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"value":"STOP NOW (stopped)","first":"STOP NOW","suffix":"!"}"#
    );
}

#[test]
fn step_input_and_when_apply_to_control_steps() {
    let workflow = WorkflowFile::new(
        r#"
input_schema: { type: object }
steps:
  - id: doubled
    input: '.numbers'
    for_each: '.'
    steps:
      - jq: '. * 2'
  - when: 'length > 5'
    switch:
      - when: 'true'
        steps:
          - jq: '"never"'
  - jq: '{doubled: ., original: $steps.doubled}'
"#,
    );

    let output = run_workflow_with(&workflow, &[r#"{"numbers": [1, 2]}"#]);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"doubled":[2,4],"original":[2,4]}"#
    );
}

#[test]
fn a_jq_expression_producing_several_values_is_an_error() {
    let workflow = WorkflowFile::new("input_schema: {type: array}\nsteps:\n  - jq: '.[]'\n");

    let output = run_workflow_with(&workflow, &["[1, 2]"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("produced 2 outputs"), "stderr: {stderr}");
}

#[test]
fn a_version_1_workflow_is_rejected_with_a_migration_hint() {
    let workflow =
        WorkflowFile::new("nodes:\n  a:\n    type: transform\n    jq: '.'\nsteps:\n  - use: a\n");

    let output = run_workflow_with(&workflow, &["x"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("migration guide"), "stderr: {stderr}");
}

#[test]
fn an_on_error_stop_takes_precedence_over_the_failing_steps_break() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - until: 'false'
    max_iterations: 1
    steps:
      - jq: 'error("failure")'
        on_error:
          - jq: '"recovered"'
          - stop: true
          - jq: 'error("later steps must not run")'
      - break: true
  - jq: 'error("later steps must not run")'
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "recovered");
}

#[test]
fn nested_control_steps_preserve_named_outputs_and_progress_counters() {
    let workflow = WorkflowFile::new(
        r#"
input_schema: { type: integer }
steps:
  - id: seed
    jq: '. + 1'
  - id: skipped
    jq: '. + 1'
    when: 'false'
  - id: route
    switch:
      - when: 'true'
        steps:
          - id: repeat
            while: '. < 3'
            max_iterations: 2
            steps:
              - id: item
                jq: '. + 1'
  - id: batch
    for_each: '[., . + 1]'
    steps:
      - id: seen
        jq: '. + 1'
  - id: forks
    parallel:
      left:
        - id: left_child
          jq: '.[0]'
      right:
        - id: right_child
          jq: '.[-1]'
  - id: report
    jq: '{value: ., steps: $steps}'
"#,
    );

    let output = run_lait_workflow(&workflow.path, "0");

    assert!(output.status.success(), "lait run failed: {output:?}");
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "value": {"left": 4, "right": 5},
            "steps": {
                "seed": 1,
                "item": 3,
                "repeat": 3,
                "route": 3,
                "seen": 5,
                "batch": [4, 5],
                "forks": {"left": 4, "right": 5},
            },
        }),
        "sequential outputs must flow outward while branch outputs stay isolated"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for progress in [
        "[2] skipped (skipped)",
        "[6] item",
        "[7] batch",
        "[10] forks",
        "[left] [1] left_child",
        "[right] [1] right_child",
        "[11] report",
    ] {
        assert!(stderr.contains(progress), "missing {progress}: {stderr}");
    }
}

#[test]
fn stop_inside_a_for_each_returns_the_stopping_item_without_running_its_output() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - for_each: '[1, 2, 3]'
    steps:
      - jq: '. + 10'
      - when: '. == 12'
        stop: true
    output: 'error("output must not run")'
  - jq: 'error("later steps must not run")'
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "12");
}
