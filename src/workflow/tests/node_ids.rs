use super::*;

#[test]
fn allows_the_same_node_to_be_used_from_multiple_steps_sites() {
    let workflow = parse_workflow(
        r#"
nodes:
  greet:
    type: prompt
    prompt: "hello: {{ input }}"
steps:
  - use: greet
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: greet
      else:
        - use: greet
"#,
    )
    .expect("reusing a node from multiple steps sites should parse");

    assert_eq!(workflow.nodes.len(), 1);
}

#[test]
fn rejects_a_site_id_colliding_with_a_different_node_id() {
    let result = parse_workflow(
        r#"
nodes:
  draft:
    type: prompt
    prompt: "draft: {{ input }}"
  summarize:
    type: transform
    jq: '.'
steps:
  - id: summarize
    use: draft
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_a_site_id_equal_to_its_own_used_node_id() {
    let result = parse_workflow(
        r#"
nodes:
  draft:
    type: prompt
    prompt: "draft: {{ input }}"
steps:
  - id: draft
    use: draft
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_a_router_site_id_colliding_with_a_different_node_id() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - id: n
    switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("collides"), "error was: {error}");
}

#[test]
fn rejects_a_nested_control_site_id_colliding_with_a_different_node_id() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    jq: '.'
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - id: n
              stop: true
"#,
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("collides"), "error was: {error}");
}

#[test]
fn rejects_a_command_with_an_empty_or_whitespace_program() {
    for program in ["", "  "] {
        let yaml = format!(
            "nodes:\n  n:\n    type: command\n    command: [\"{program}\"]\nsteps:\n  - use: n\n"
        );
        let error = parse_workflow(&yaml).unwrap_err().to_string();
        assert!(error.contains("command[0]"), "error was: {error}");
    }
}
