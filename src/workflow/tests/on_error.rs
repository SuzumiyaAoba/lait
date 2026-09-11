use super::*;

#[test]
fn validates_steps_nested_inside_on_error() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
    on_error:
      steps:
        - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn on_error_inherits_the_failing_steps_loop_context_for_break() {
    let workflow = parse_workflow(
        r#"
nodes:
  caller:
    type: prompt
    prompt: "{{ input }}"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - use: caller
          on_error:
            steps:
              - break: true
"#,
    );
    assert!(workflow.is_ok());
}

#[test]
fn parses_a_node_with_write_file() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    write_file: out.txt
steps:
  - use: n
"#,
    )
    .expect("workflow with write_file should parse");

    assert_eq!(
        workflow.nodes["n"].settings().write_file,
        Some(std::path::Path::new("out.txt"))
    );
}
