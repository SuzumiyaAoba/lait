use super::eval_and_compile::{as_loop, as_parallel};
use super::*;

#[test]
fn parses_a_parallel_without_join() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: "."
steps:
  - parallel:
      branches:
        - steps:
            - use: n
        - steps:
            - use: n
"#,
    )
    .expect("workflow with a parallel step without join should parse");

    assert!(as_parallel(&workflow.steps[0]).join.is_none());
}

#[test]
fn parallel_branch_label_defaults_to_branch_n() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: "."
steps:
  - parallel:
      branches:
        - steps:
            - use: n
        - id: named
          steps:
            - use: n
"#,
    )
    .expect("workflow with a parallel step should parse");

    let branches = &as_parallel(&workflow.steps[0]).branches;
    assert_eq!(branches[0].label(0), "branch-1");
    assert_eq!(branches[1].label(1), "named");
}

#[test]
fn rejects_a_parallel_with_empty_branches() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_parallel_branch_with_empty_steps() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_parallel_with_duplicate_branch_ids() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: "."
steps:
  - parallel:
      branches:
        - id: same
          steps:
            - use: n
        - id: same
          steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_parallel_combined_with_use() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
    parallel:
      branches:
        - steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_parallel_combined_with_stop() {
    let result = parse_workflow(
        r#"
steps:
  - stop: true
    parallel:
      branches:
        - steps:
            - stop: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_step_with_both_switch_and_parallel() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: "."
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: n
    parallel:
      branches:
        - steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_loop_with_while_and_max_iterations() {
    let workflow = parse_workflow(
        r#"
nodes:
  bump:
    type: transform
    jq: '.score += 1'
steps:
  - id: refine
    loop:
      while: '.score < 3'
      max_iterations: 5
      steps:
        - use: bump
"#,
    )
    .expect("workflow with a while loop should parse");

    let loop_def = as_loop(&workflow.steps[0]);
    assert!(
        matches!(&loop_def.condition, crate::workflow::LoopCondition::While(filter) if filter == ".score < 3")
    );
    assert_eq!(loop_def.max_iterations.get(), 5);
}
