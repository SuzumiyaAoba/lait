mod support;

use support::{WorkflowFile, run_lait_workflow};

#[test]
fn ask_uses_its_default_when_stdin_is_not_a_terminal() {
    // `run_lait_workflow` (like every `Command::output()` invocation) never
    // gives the child process a real terminal, so this always exercises the
    // non-interactive path.
    let workflow = WorkflowFile::new(
        r#"
steps:
  - ask: "proceed?"
    default: "yes"
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "yes");
}

#[test]
fn ask_fails_when_stdin_is_not_a_terminal_and_no_default_is_set() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - ask: "proceed?"
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not an interactive terminal"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("default"), "stderr: {stderr}");
}

#[test]
fn ask_records_its_answer_for_a_later_steps_jq_filter() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - id: confirm
    ask: "proceed?"
    default: "yes"
  - jq: '{answer: $steps.confirm}'
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"answer":"yes"}"#
    );
}

#[test]
fn ask_output_maps_its_answer() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - ask: "how many?"
    default: "3"
    output: 'tonumber * 2'
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
fn ask_renders_its_question_from_the_input() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - ask: "{{ input }}"
    default: "ok"
"#,
    );

    let output = run_lait_workflow(&workflow.path, "question text");

    assert!(output.status.success(), "lait run failed: {output:?}");
}

#[test]
fn ask_rejects_a_default_that_is_not_one_of_its_choices() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - ask: "proceed?"
    choices: ["yes", "no"]
    default: "maybe"
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be one of 'choices'"),
        "stderr: {stderr}"
    );
}

#[test]
fn ask_rejects_an_empty_question() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - ask: "  "
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("non-empty question"), "stderr: {stderr}");
}

#[test]
fn ask_rejects_an_empty_choices_list() {
    for choices in ["[]", "['']"] {
        let workflow = WorkflowFile::new(&format!(
            "steps:\n  - ask: \"proceed?\"\n    choices: {choices}\n"
        ));

        let output = run_lait_workflow(&workflow.path, "null");

        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("choices"), "stderr: {stderr}");
    }
}

#[test]
fn ask_is_rejected_inside_a_parallel_branch() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - parallel:
      confirm:
        - ask: "proceed?"
          default: "yes"
      other:
        - jq: '.'
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("'ask'"), "stderr: {stderr}");
    assert!(stderr.contains("parallel"), "stderr: {stderr}");
}

#[test]
fn ask_is_rejected_inside_a_concurrent_for_each_body() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - for_each: '[1, 2]'
    max_concurrency: 2
    steps:
      - ask: "proceed?"
        default: "yes"
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("'ask'"), "stderr: {stderr}");
}

#[test]
fn ask_is_allowed_inside_a_sequential_for_each_body() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - for_each: '[1, 2]'
    steps:
      - ask: "proceed?"
        default: "yes"
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"["yes","yes"]"#
    );
}
