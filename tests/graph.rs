mod support;

use support::{WorkflowFile, test_command};

#[test]
fn graph_defaults_to_mermaid_and_wires_sequential_steps() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  extract:
    type: prompt
    prompt: "{{ input }}"
  greet:
    type: prompt
    prompt: "{{ steps.extract }}"
steps:
  - id: extract
    use: extract
  - use: greet
"#,
    );

    let output = test_command()
        .args([
            "graph",
            workflow.path.to_str().expect("workflow path is utf-8"),
        ])
        .output()
        .expect("failed to execute lait graph");

    assert!(output.status.success(), "lait graph failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("flowchart TD\n"), "stdout: {stdout}");
    assert!(stdout.contains("[extract]"), "stdout: {stdout}");
    assert!(stdout.contains("[greet]"), "stdout: {stdout}");
    assert!(stdout.contains("type: prompt"), "stdout: {stdout}");
}

#[test]
fn graph_dot_format_emits_a_digraph() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  a:
    type: transform
    jq: '.'
steps:
  - use: a
"#,
    );

    let output = test_command()
        .args([
            "graph",
            workflow.path.to_str().expect("workflow path is utf-8"),
            "--format",
            "dot",
        ])
        .output()
        .expect("failed to execute lait graph --format dot");

    assert!(output.status.success(), "lait graph failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("digraph workflow {\n"),
        "stdout: {stdout}"
    );
    assert!(stdout.trim_end().ends_with('}'), "stdout: {stdout}");
    assert!(stdout.contains("shape=box"), "stdout: {stdout}");
}

#[test]
fn graph_labels_a_switch_edge_with_its_when_condition() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  a:
    type: transform
    jq: '.'
  b:
    type: transform
    jq: '.'
steps:
  - switch:
      cases:
        - id: yes
          when: ".flag"
          steps:
            - use: a
      else:
        - use: b
"#,
    );

    let output = test_command()
        .args([
            "graph",
            workflow.path.to_str().expect("workflow path is utf-8"),
        ])
        .output()
        .expect("failed to execute lait graph");

    assert!(output.status.success(), "lait graph failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("switch"), "stdout: {stdout}");
    assert!(stdout.contains("yes: .flag"), "stdout: {stdout}");
    assert!(stdout.contains("else"), "stdout: {stdout}");
}

#[test]
fn graph_groups_a_loop_body_into_a_subgraph() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  a:
    type: transform
    jq: '.'
steps:
  - loop:
      while: ".continue"
      max_iterations: 5
      steps:
        - use: a
"#,
    );

    let output = test_command()
        .args([
            "graph",
            workflow.path.to_str().expect("workflow path is utf-8"),
        ])
        .output()
        .expect("failed to execute lait graph");

    assert!(output.status.success(), "lait graph failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("loop: while .continue"), "stdout: {stdout}");
    assert!(stdout.contains("max_iterations: 5"), "stdout: {stdout}");
    assert!(stdout.contains("subgraph"), "stdout: {stdout}");
    assert!(stdout.contains("loop body"), "stdout: {stdout}");
}

#[test]
fn graph_shows_a_workflow_node_as_a_single_reference_without_expanding_it() {
    let sub =
        WorkflowFile::new("nodes:\n  a:\n    type: transform\n    jq: '.'\nsteps:\n  - use: a\n");
    let sub_path = sub.path.to_str().expect("sub workflow path is utf-8");
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  call_sub:
    type: workflow
    workflow: "{sub_path}"
steps:
  - use: call_sub
"#,
    ));

    let output = test_command()
        .args([
            "graph",
            workflow.path.to_str().expect("workflow path is utf-8"),
        ])
        .output()
        .expect("failed to execute lait graph");

    assert!(output.status.success(), "lait graph failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("type: workflow"), "stdout: {stdout}");
    assert!(stdout.contains(sub_path), "stdout: {stdout}");
}

#[test]
fn graph_fails_on_an_invalid_workflow_file() {
    let workflow = WorkflowFile::new("nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps: []\n");

    let output = test_command()
        .args([
            "graph",
            workflow.path.to_str().expect("workflow path is utf-8"),
        ])
        .output()
        .expect("failed to execute lait graph");

    assert!(!output.status.success());
}
