use super::eval_and_compile::{as_parallel, as_switch};
use super::*;

#[test]
fn parses_a_switch_without_else() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    )
    .expect("workflow with a switch without else should parse");

    assert!(as_switch(&workflow.steps[0]).else_steps.is_none());
}

#[test]
fn rejects_a_switch_with_empty_cases() {
    let result = parse_workflow(
        r#"
steps:
  - switch:
      cases: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_case_with_empty_steps() {
    let result = parse_workflow(
        r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_with_an_empty_else() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: n
      else: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_combined_with_use() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
    switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_combined_with_when() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - when: 'true'
    switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_combined_with_on_error() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - on_error:
      steps:
        - use: n
    switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_a_switch_case() {
    let result = parse_workflow(
        r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_parallel_with_branches_and_join() {
    let workflow = parse_workflow(
        r#"
nodes:
  a:
    type: prompt
    prompt: "a: {{ input }}"
  b:
    type: prompt
    prompt: "b: {{ input }}"
steps:
  - id: fan-out
    parallel:
      branches:
        - id: a
          steps:
            - use: a
        - id: b
          steps:
            - use: b
      join: '.a + .b'
"#,
    )
    .expect("workflow with a parallel step should parse");

    let parallel = as_parallel(&workflow.steps[0]);
    assert_eq!(parallel.branches.len(), 2);
    assert_eq!(parallel.branches[0].id.as_deref(), Some("a"));
    assert_eq!(parallel.join.as_deref(), Some(".a + .b"));
}
