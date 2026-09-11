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
nodes:
  call:
    type: prompt
    prompt: "{{{{ input }}}}"
  never:
    type: transform
    jq: '"should not run"'
steps:
  - id: call
    use: call
    stop: true
  - id: never
    use: never
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
nodes:
  read_route:
    type: transform
    jq: '$steps.route'
steps:
  - loop:
      until: 'true'
      max_iterations: 1
      steps:
        - id: route
          switch:
            cases:
              - when: 'true'
                steps:
                  - break: true
  - use: read_route
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
nodes:
  fail:
    type: command
    command: ["sh", "-c", "exit 1"]
  recover:
    type: transform
    jq: '"recovered"'
  read_failure:
    type: transform
    jq: '$steps.failed'
steps:
  - loop:
      until: 'true'
      max_iterations: 1
      steps:
        - id: failed
          use: fail
          on_error:
            steps:
              - use: recover
              - break: true
  - use: read_failure
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
nodes:
  bump:
    type: transform
    jq: '.n += 1'
steps:
  - loop:
      until: '.n >= 10'
      max_iterations: 5
      steps:
        - use: bump
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
nodes:
  passthrough:
    type: transform
    jq: '.'
steps:
  - for_each:
      items: '.items'
      steps:
        - when: '. == 2'
          break: true
        - use: passthrough
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
nodes:
  bump:
    type: transform
    jq: '.n += 1'
  never:
    type: transform
    jq: '"should not run"'
steps:
  - loop:
      until: '.n >= 10'
      max_iterations: 5
      steps:
        - use: bump
        - when: '.n == 2'
          stop: true
  - use: never
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), r#"{"n":2}"#);
}

#[test]
fn stop_inside_a_for_each_returns_the_stopping_item_without_running_join() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  increment:
    type: transform
    jq: '. + 10'
  never:
    type: transform
    jq: 'error("later steps must not run")'
steps:
  - for_each:
      items: '[1, 2, 3]'
      steps:
        - use: increment
        - when: '. == 12'
          stop: true
      join: 'error("join must not run")'
  - use: never
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "12");
}

#[test]
fn an_on_error_stop_takes_precedence_over_the_failing_steps_break() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  fail:
    type: transform
    jq: 'error("failure")'
  recover:
    type: transform
    jq: '"recovered"'
  never:
    type: transform
    jq: 'error("later steps must not run")'
steps:
  - loop:
      until: 'false'
      max_iterations: 1
      steps:
        - use: fail
          break: true
          on_error:
            steps:
              - use: recover
                stop: true
              - use: never
  - use: never
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "recovered");
}

#[test]
fn nested_routers_preserve_named_outputs_and_progress_counters() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  increment:
    type: transform
    jq: '. + 1'
  first:
    type: transform
    jq: '.[0]'
  last:
    type: transform
    jq: '.[-1]'
  inspect:
    type: transform
    jq: '{value: ., steps: $steps}'
steps:
  - id: seed
    use: increment
  - id: skipped
    use: increment
    when: 'false'
  - id: route
    switch:
      cases:
        - when: 'true'
          steps:
            - id: repeat
              loop:
                while: '. < 3'
                max_iterations: 2
                steps:
                  - id: item
                    use: increment
  - id: batch
    for_each:
      items: '[., . + 1]'
      steps:
        - id: seen
          use: increment
  - id: forks
    parallel:
      branches:
        - id: left
          steps:
            - id: left_child
              use: first
        - id: right
          steps:
            - id: right_child
              use: last
  - id: report
    use: inspect
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
