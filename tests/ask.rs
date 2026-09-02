mod support;

use support::{WorkflowFile, run_lait_workflow};

#[test]
fn ask_uses_its_default_when_stdin_is_not_a_terminal() {
    // `run_lait_workflow` (like every `Command::output()` invocation) never
    // gives the child process a real terminal, so this always exercises the
    // non-interactive path.
    let workflow = WorkflowFile::new(
        r#"
nodes:
  confirm:
    type: ask
    prompt: "proceed?"
    default: "yes"
steps:
  - use: confirm
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
nodes:
  confirm:
    type: ask
    prompt: "proceed?"
steps:
  - use: confirm
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
nodes:
  confirm:
    type: ask
    prompt: "proceed?"
    default: "yes"
  read_confirm:
    type: transform
    jq: '$steps.confirm'
steps:
  - id: confirm
    use: confirm
  - use: read_confirm
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "yes");
}

#[test]
fn ask_applies_jq_to_its_answer() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  confirm:
    type: ask
    prompt: "how many?"
    default: "3"
    jq: 'tonumber * 2'
steps:
  - use: confirm
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
fn ask_rejects_a_default_that_is_not_one_of_its_choices() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  confirm:
    type: ask
    prompt: "proceed?"
    choices: [yes, no]
    default: "maybe"
steps:
  - use: confirm
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("which is not one of its 'choices'"),
        "stderr: {stderr}"
    );
}

#[test]
fn ask_rejects_an_empty_prompt() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  confirm:
    type: ask
    prompt: ""
steps:
  - use: confirm
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("empty 'prompt'"), "stderr: {stderr}");
}

#[test]
fn ask_rejects_an_empty_choices_list() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  confirm:
    type: ask
    prompt: "proceed?"
    choices: []
steps:
  - use: confirm
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("choices"), "stderr: {stderr}");
}

#[test]
fn ask_is_rejected_inside_a_parallel_branch() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  confirm:
    type: ask
    prompt: "proceed?"
    default: "yes"
  noop:
    type: transform
    jq: '.'
steps:
  - parallel:
      branches:
        - steps:
            - use: confirm
        - steps:
            - use: noop
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("'type: ask'"), "stderr: {stderr}");
    assert!(stderr.contains("parallel"), "stderr: {stderr}");
}

#[test]
fn ask_is_rejected_inside_a_concurrent_for_each_body() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  confirm:
    type: ask
    prompt: "proceed?"
    default: "yes"
steps:
  - for_each:
      items: '[1, 2]'
      max_concurrency: 2
      steps:
        - use: confirm
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("'type: ask'"), "stderr: {stderr}");
}

#[test]
fn ask_is_allowed_inside_a_sequential_for_each_body() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  confirm:
    type: ask
    prompt: "proceed?"
    default: "yes"
steps:
  - for_each:
      items: '[1, 2]'
      steps:
        - use: confirm
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("yes"), "stdout: {stdout}");
}
