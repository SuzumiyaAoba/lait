use super::*;

#[test]
fn rejects_a_retry_with_no_max_attempts() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    retry:
      delay_seconds: 1
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_retry_with_max_attempts_zero() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    retry:
      max_attempts: 0
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_timeout_of_zero() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    timeout: 0
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_node_with_temperature_top_p_and_max_tokens() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    model: local
    temperature: 0.7
    top_p: 0.9
    max_tokens: 256
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    )
    .expect("workflow should parse");

    assert_eq!(workflow.nodes["n"].settings().temperature, Some(0.7));
    assert_eq!(workflow.nodes["n"].settings().top_p, Some(0.9));
    assert_eq!(workflow.nodes["n"].settings().max_tokens, Some(256));
}

#[test]
fn parses_workflow_default_temperature_top_p_and_max_tokens() {
    let workflow = parse_workflow(
        r#"
default:
  model: local
  temperature: 0.5
  top_p: 0.8
  max_tokens: 128
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    )
    .expect("workflow should parse");

    assert_eq!(workflow.default.temperature, Some(0.5));
    assert_eq!(workflow.default.top_p, Some(0.8));
    assert_eq!(workflow.default.max_tokens, Some(128));
}

#[test]
fn rejects_a_node_with_an_out_of_range_temperature() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    temperature: 2.5
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_node_with_an_out_of_range_top_p() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    top_p: 1.5
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_node_with_a_zero_max_tokens() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    max_tokens: 0
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_default_with_an_out_of_range_temperature() {
    let result = parse_workflow(
        r#"
default:
  temperature: -0.1
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
fn rejects_an_on_error_with_an_empty_steps_list() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
    on_error:
      steps: []
"#,
    );
    assert!(result.is_err());
}
