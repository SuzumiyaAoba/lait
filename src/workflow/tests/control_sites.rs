use super::eval_and_compile::as_switch;
use super::*;

#[test]
fn rejects_a_workflow_combined_with_switch() {
    let result = parse_workflow(
        r#"
nodes:
  sub:
    type: workflow
    workflow: sub.yml
steps:
  - use: sub
    switch:
      cases:
        - when: 'true'
          steps:
            - use: sub
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_a_workflow_step_with_when_jq_and_stop() {
    let result = parse_workflow(
        r#"
nodes:
  sub:
    type: workflow
    workflow: sub.yml
    jq: '.'
steps:
  - when: 'true'
    use: sub
    stop: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_unknown_top_level_field() {
    let result = parse_workflow(
        r#"
unexpected: true
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_step_with_a_when_guard() {
    let workflow = parse_workflow(
        r#"
nodes:
  maybe:
    type: prompt
    prompt: "{{ input }}"
steps:
  - id: maybe
    when: '. != null'
    use: maybe
"#,
    )
    .expect("workflow with a 'when' guard should parse");

    assert_eq!(workflow.steps[0].when(), Some(". != null"));
}

#[test]
fn parses_a_switch_with_cases_and_else() {
    let workflow = parse_workflow(
        r#"
nodes:
  escalate:
    type: prompt
    prompt: "escalate: {{ input }}"
  reply:
    type: prompt
    prompt: "reply: {{ input }}"
  summarize:
    type: transform
    jq: ".summary"
steps:
  - id: route
    switch:
      cases:
        - id: high
          when: '.severity == "high"'
          steps:
            - use: escalate
        - when: '.severity == "medium"'
          steps:
            - use: reply
      else:
        - use: summarize
"#,
    )
    .expect("workflow with a switch should parse");

    let switch = as_switch(&workflow.steps[0]);
    assert_eq!(switch.cases.len(), 2);
    assert_eq!(switch.cases[0].id.as_deref(), Some("high"));
    assert!(switch.else_steps.is_some());
}
