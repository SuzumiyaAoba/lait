use super::*;

#[test]
fn rejects_a_workflow_default_retry_with_no_max_attempts() {
    let result = parse_workflow(
        r#"
default:
  retry:
    delay_seconds: 1
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
fn rejects_a_workflow_default_retry_with_max_attempts_zero() {
    let result = parse_workflow(
        r#"
default:
  retry:
    max_attempts: 0
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
fn rejects_a_retry_with_a_negative_backoff() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    retry:
      max_attempts: 2
      backoff: -1.0
steps:
  - use: n
"#,
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("backoff"), "error was: {error}");
}

#[test]
fn rejects_a_retry_with_a_non_finite_backoff() {
    let result = parse_workflow(
        r#"
default:
  retry:
    max_attempts: 2
    backoff: .inf
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("backoff"), "error was: {error}");
}

#[test]
fn rejects_a_workflow_default_timeout_of_zero() {
    let result = parse_workflow(
        r#"
default:
  timeout: 0
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
fn rejects_a_node_max_tool_rounds_of_zero() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    type: prompt\n    prompt: hi\n    max_tool_rounds: 0\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_default_max_tool_rounds_of_zero() {
    let result = parse_workflow("default:\n  max_tool_rounds: 0\nsteps:\n  - use: n\n");
    assert!(result.is_err());
}

#[test]
fn rejects_a_use_of_an_undefined_node() {
    let result = parse_workflow(
        r#"
steps:
  - use: missing
"#,
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("missing"), "error was: {error}");
}
