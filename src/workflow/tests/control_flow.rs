use super::eval_and_compile::{as_foreach, as_loop};
use super::*;

#[test]
fn parses_a_step_with_break_inside_a_loop() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - when: '.done'
          break: true
        - use: n
"#,
    )
    .expect("workflow with 'break' inside a loop should parse");

    let loop_def = as_loop(&workflow.steps[0]);
    assert_eq!(loop_def.steps[0].control(), crate::workflow::Control::Break);
}

#[test]
fn parses_a_step_with_break_inside_a_for_each() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - for_each:
      items: '.items'
      steps:
        - when: '.done'
          break: true
        - use: n
"#,
    )
    .expect("workflow with 'break' inside a for_each should parse");

    let for_each = as_foreach(&workflow.steps[0]);
    assert_eq!(for_each.steps[0].control(), crate::workflow::Control::Break);
}

#[test]
fn allows_break_inside_a_loop_nested_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - loop:
                until: 'true'
                max_iterations: 3
                steps:
                  - break: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_break_at_the_top_level() {
    let result = parse_workflow(
        r#"
steps:
  - break: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_break_directly_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - break: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_stop_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - stop: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_stop_inside_a_loop_nested_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - loop:
                until: 'true'
                max_iterations: 3
                steps:
                  - stop: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_both_stop_and_break_on_the_same_step() {
    let result = parse_workflow(
        r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - stop: true
          break: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_step_with_neither_an_action_nor_stop_or_break() {
    let result = parse_workflow(
        r#"
steps:
  - id: empty
    when: 'true'
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_use_combined_with_stop() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
    stop: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn allows_a_bare_when_and_stop_with_no_use() {
    let result = parse_workflow(
        r#"
steps:
  - when: '.ready'
    stop: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_on_error_without_use() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - stop: true
    on_error:
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_step_with_retry_timeout_and_on_error() {
    let workflow = parse_workflow(
        r#"
nodes:
  call:
    type: prompt
    prompt: "{{ input }}"
    timeout: 30
    retry:
      max_attempts: 3
      delay_seconds: 1
      backoff: 2.0
  fallback:
    type: transform
    jq: '{ fallback: .error }'
steps:
  - id: call
    use: call
    on_error:
      steps:
        - use: fallback
"#,
    )
    .expect("workflow with retry/timeout/on_error should parse");

    let node = &workflow.nodes["call"];
    assert_eq!(node.settings().timeout, Some(30));
    let retry = node.settings().retry.unwrap();
    assert_eq!(retry.max_attempts, Some(3));
    assert_eq!(retry.delay_seconds, Some(1));
    assert_eq!(retry.backoff, Some(2.0));
    assert_eq!(workflow.steps[0].on_error().unwrap().steps.len(), 1);
}
