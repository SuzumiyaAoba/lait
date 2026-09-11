use super::eval_and_compile::{as_foreach, as_loop};
use super::*;

#[test]
fn parses_a_loop_with_until() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - loop:
      until: '.valid == true'
      max_iterations: 3
      steps:
        - use: n
"#,
    )
    .expect("workflow with an until loop should parse");

    let loop_def = as_loop(&workflow.steps[0]);
    assert!(
        matches!(&loop_def.condition, crate::workflow::LoopCondition::Until(filter) if filter == ".valid == true")
    );
}

#[test]
fn rejects_a_loop_with_both_while_and_until() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - loop:
      while: 'true'
      until: 'true'
      max_iterations: 3
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_with_neither_while_nor_until() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - loop:
      max_iterations: 3
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_with_no_max_iterations() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - loop:
      until: 'true'
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_with_max_iterations_zero() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - loop:
      until: 'true'
      max_iterations: 0
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_with_empty_steps() {
    let result = parse_workflow(
        r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_combined_with_use() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
    loop:
      until: 'true'
      max_iterations: 3
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_a_loop() {
    let result = parse_workflow(
        r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_for_each_with_items_and_join() {
    let workflow = parse_workflow(
        r#"
nodes:
  bump:
    type: transform
    jq: '. + 1'
steps:
  - id: process
    for_each:
      items: '.items'
      steps:
        - use: bump
      join: 'map(. * 2)'
"#,
    )
    .expect("workflow with a for_each should parse");

    let for_each = as_foreach(&workflow.steps[0]);
    assert_eq!(for_each.items, ".items");
    assert_eq!(for_each.join.as_deref(), Some("map(. * 2)"));
}

#[test]
fn parses_a_for_each_without_join() {
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
        - use: n
"#,
    )
    .expect("workflow with a for_each without join should parse");

    assert!(as_foreach(&workflow.steps[0]).join.is_none());
}

#[test]
fn rejects_a_for_each_with_empty_steps() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.items'
      steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_for_each_with_max_concurrency() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - for_each:
      items: '.items'
      max_concurrency: 4
      steps:
        - use: n
"#,
    )
    .expect("workflow with a for_each max_concurrency should parse");

    assert_eq!(as_foreach(&workflow.steps[0]).max_concurrency, Some(4));
}

#[test]
fn rejects_a_for_each_with_max_concurrency_zero() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - for_each:
      items: '.items'
      max_concurrency: 0
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_break_inside_a_for_each_with_max_concurrency_above_one() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.items'
      max_concurrency: 2
      steps:
        - break: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_stop_inside_a_for_each_with_max_concurrency_above_one() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.items'
      max_concurrency: 2
      steps:
        - stop: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_break_inside_a_sequential_for_each_nested_in_a_concurrent_one() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.outer'
      max_concurrency: 2
      steps:
        - for_each:
            items: '.inner'
            steps:
              - break: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_use_of_a_write_file_node_inside_a_for_each_with_max_concurrency_above_one() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
    write_file: out.txt
steps:
  - for_each:
      items: '.items'
      max_concurrency: 2
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_use_of_a_write_file_node_inside_a_sequential_for_each() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
    write_file: out.txt
steps:
  - for_each:
      items: '.items'
      steps:
        - use: n
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_use_of_a_write_file_node_inside_a_sequential_for_each_nested_in_a_concurrent_one() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
    write_file: out.txt
steps:
  - for_each:
      items: '.outer'
      max_concurrency: 2
      steps:
        - for_each:
            items: '.inner'
            steps:
              - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_for_each_combined_with_when() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - when: 'true'
    for_each:
      items: '.items'
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_a_for_each() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.items'
      steps:
        - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_step_with_both_loop_and_for_each() {
    let result = parse_workflow(
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
        - use: n
    for_each:
      items: '.items'
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_step_with_stop() {
    let workflow = parse_workflow(
        r#"
steps:
  - id: done
    when: '.ready'
    stop: true
"#,
    )
    .expect("workflow with a top-level 'stop' should parse");

    assert_eq!(workflow.steps[0].control(), crate::workflow::Control::Stop);
}
