mod support;

use support::{WorkflowFile, test_command};

#[test]
fn graph_defaults_to_mermaid_and_wires_sequential_steps() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - id: extract
    prompt: "{{ input }}"
  - id: greet
    prompt: "{{ steps.extract }}"
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
    assert!(stdout.contains("<br/>prompt"), "stdout: {stdout}");
}

#[test]
fn graph_dot_format_emits_a_digraph() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - id: a
    jq: '.'
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
steps:
  - switch:
      - when: ".flag"
        steps:
          - id: a
            jq: '.'
    else:
      - id: b
        jq: '.'
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
    assert!(stdout.contains("case 1: .flag"), "stdout: {stdout}");
    assert!(stdout.contains("else"), "stdout: {stdout}");
}

#[test]
fn graph_groups_a_loop_body_into_a_subgraph() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - while: ".continue"
    max_iterations: 5
    steps:
      - id: a
        jq: '.'
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
    assert!(stdout.contains("while .continue"), "stdout: {stdout}");
    assert!(stdout.contains("max_iterations: 5"), "stdout: {stdout}");
    // Mermaid's subgraph grammar is `subgraph id[title]` — a bare quoted
    // title with no id is only accepted by some renderers.
    assert!(stdout.contains("subgraph sg0["), "stdout: {stdout}");
    assert!(stdout.contains("loop body"), "stdout: {stdout}");
}

#[test]
fn graph_shows_a_workflow_node_as_a_single_reference_without_expanding_it() {
    let sub = WorkflowFile::new("steps:\n  - id: a\n    jq: '.'\n");
    let sub_path = sub.path.to_str().expect("sub workflow path is utf-8");
    let workflow = WorkflowFile::new(&format!(
        r#"
steps:
  - id: call_sub
    workflow: "{sub_path}"
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
    assert!(stdout.contains("<br/>workflow<br/>"), "stdout: {stdout}");
    assert!(stdout.contains(sub_path), "stdout: {stdout}");
}

#[test]
fn graph_fails_on_an_invalid_workflow_file() {
    let workflow = WorkflowFile::new("steps: []\n");

    let output = test_command()
        .args([
            "graph",
            workflow.path.to_str().expect("workflow path is utf-8"),
        ])
        .output()
        .expect("failed to execute lait graph");

    assert!(!output.status.success());
}
