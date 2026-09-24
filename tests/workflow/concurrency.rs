use super::*;

#[test]
fn concurrent_for_each_preserves_item_order_in_its_results_regardless_of_completion_order() {
    let workflow = WorkflowFile::new(
        r#"
input_schema: {type: object}
steps:
  - for_each: '.items'
    max_concurrency: 3
    steps:
      - id: times10
        jq: '. * 10'
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[1,2,3]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[10,20,30]");
}

#[test]
fn concurrent_for_each_calls_the_model_once_per_item() {
    let server = MockServer::start_sequence(&[
        ("200 OK", CHAT_COMPLETION_BODY),
        ("200 OK", CHAT_COMPLETION_BODY),
        ("200 OK", CHAT_COMPLETION_BODY),
    ]);
    let workflow = WorkflowFile::new(&format!(
        r#"
input_schema: {{type: object}}
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
steps:
  - for_each: '.items'
    max_concurrency: 3
    steps:
      - id: echo
        prompt: "{{{{ input }}}}"
    output: 'length'
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, r#"{"items":["a","b","c"]}"#);
    server.receive_request();
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "3");
}

#[test]
fn concurrent_for_each_rejects_break_and_stop_in_its_steps_at_parse_time() {
    let break_workflow = WorkflowFile::new(
        r#"
input_schema: {type: object}
steps:
  - for_each: '.items'
    max_concurrency: 2
    steps:
      - break: true
"#,
    );
    let output = run_lait_workflow(&break_workflow.path, r#"{"items":[1]}"#);
    assert!(
        !output.status.success(),
        "expected 'break' inside a concurrent for_each to be rejected"
    );

    let stop_workflow = WorkflowFile::new(
        r#"
input_schema: {type: object}
steps:
  - for_each: '.items'
    max_concurrency: 2
    steps:
      - stop: true
"#,
    );
    let output = run_lait_workflow(&stop_workflow.path, r#"{"items":[1]}"#);
    assert!(
        !output.status.success(),
        "expected 'stop' inside a concurrent for_each to be rejected"
    );
}

#[test]
fn concurrent_parent_rejects_interactive_child_before_child_side_effects() {
    let dir = support::ScratchDir::new();
    dir.write("child.yml", "steps:\n  - id: first\n    jq: '.'\n  - write: touched.txt\n  - id: question\n    ask: continue\n    default: yes\n");
    let root = dir.write("root.yml", "steps:\n  - parallel:\n      branch-1:\n        - id: child\n          workflow: child.yml\n");
    let output = test_command()
        .current_dir(dir.path())
        .arg("run")
        .arg(root)
        .args(["input", "--no-config", "--no-env", "--no-history"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("concurrent") && stderr.contains("ask"),
        "{stderr}"
    );
    assert!(
        !dir.path().join("touched.txt").exists(),
        "child effects ran before preflight"
    );
}

#[test]
fn concurrent_for_each_restrictions_cross_workflow_file_boundaries() {
    let dir = support::ScratchDir::new();
    dir.write(
        "grandchild.yml",
        "steps:\n  - id: save\n    jq: '.'\n  - write: result.txt\n",
    );
    dir.write(
        "child.yml",
        "steps:\n  - id: next\n    workflow: grandchild.yml\n",
    );
    let root = dir.write("root.yml", "input_schema: {type: array}\nsteps:\n  - for_each: '.'\n    max_concurrency: 2\n    steps:\n      - id: child\n        workflow: child.yml\n");
    let output = test_command()
        .current_dir(dir.path())
        .arg("run")
        .arg(root)
        .args(["[1,2]", "--no-config", "--no-env", "--no-history"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("concurrent") && stderr.contains("fixed path"),
        "{stderr}"
    );
    assert!(!dir.path().join("result.txt").exists());
}

#[test]
fn a_child_workflow_can_stop_locally_inside_a_parallel_parent() {
    let dir = support::ScratchDir::new();
    dir.write("child.yml", "steps: [{stop: true}]\n");
    let root = dir.write("root.yml", "steps:\n  - parallel:\n      a:\n        - id: child\n          workflow: child.yml\n      b:\n        - workflow: child.yml\n");
    let output = test_command()
        .current_dir(dir.path())
        .arg("run")
        .arg(root)
        .args(["input", "--no-config", "--no-env", "--no-history"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value, serde_json::json!({"a":"input", "b":"input"}));
}
