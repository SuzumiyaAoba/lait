use super::*;

#[test]
fn a_workflow_step_runs_a_sub_workflow_and_uses_its_output() {
    let sub = WorkflowFile::new(
        r#"
steps:
  - id: add_one
    jq: '. + 1'
"#,
    );
    let sub_name = sub.path.file_name().unwrap().to_str().unwrap();
    let parent = WorkflowFile::new(&format!(
        r#"
input_schema: {{type: integer}}
steps:
  - id: sub
    workflow: {sub_name}
    output: '. * 2'
"#
    ));

    let output = run_lait_workflow(&parent.path, "1");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4");
}

#[test]
fn a_sub_workflows_falls_back_to_the_callers_default_model() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let sub = WorkflowFile::new(
        r#"
steps:
  - id: echo
    prompt: "{{ input }}"
"#,
    );
    let sub_name = sub.path.file_name().unwrap().to_str().unwrap();
    let parent = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
steps:
  - id: sub
    workflow: {sub_name}
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&parent.path, "hello");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn a_sub_workflows_own_default_model_takes_precedence_over_the_callers() {
    let caller_server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let sub_server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"sub-model","choices":[{"index":0,"message":{"role":"assistant","content":"sub response"},"finish_reason":"stop"}]}"#,
    );
    let sub = WorkflowFile::new(&format!(
        r#"
default:
  model: sub-model
models:
  sub-model:
    - provider:
        base_url: "{}"
      model_id: sub-model-id
steps:
  - id: echo
    prompt: "{{{{ input }}}}"
"#,
        sub_server.base_url
    ));
    let sub_name = sub.path.file_name().unwrap().to_str().unwrap();
    let parent = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
steps:
  - id: sub
    workflow: {sub_name}
"#,
        caller_server.base_url
    ));

    let output = run_lait_workflow(&parent.path, "hello");
    let request = sub_server.receive_request();
    sub_server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "sub response"
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"sub-model-id""#),
        "request body: {body}"
    );
}

#[test]
fn a_sub_workflows_named_step_outputs_are_isolated_from_the_caller_in_both_directions() {
    let sub = WorkflowFile::new(
        r#"
steps:
  - id: inner
    jq: '$steps.outer'
"#,
    );
    let sub_name = sub.path.file_name().unwrap().to_str().unwrap();
    let parent = WorkflowFile::new(&format!(
        r#"
steps:
  - id: outer
    jq: '{{ from_outer: true }}'
  - id: sub
    workflow: {sub_name}
  - id: read_inner
    jq: '$steps.inner'
"#
    ));

    let output = run_lait_workflow(&parent.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "null",
        "expected neither direction of {{ steps.* }} to cross the workflow-call boundary"
    );
}

#[test]
fn a_workflow_step_cycle_is_rejected() {
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let a_path = std::env::temp_dir().join(format!("lait-test-cycle-a-{unique}.yml"));
    let b_path = std::env::temp_dir().join(format!("lait-test-cycle-b-{unique}.yml"));

    std::fs::write(
        &a_path,
        format!(
            "steps:\n  - id: sub\n    workflow: ./{}\n",
            b_path.file_name().unwrap().to_str().unwrap()
        ),
    )
    .expect("failed to write cycle test file a");
    std::fs::write(
        &b_path,
        format!(
            "steps:\n  - id: sub\n    workflow: ./{}\n",
            a_path.file_name().unwrap().to_str().unwrap()
        ),
    )
    .expect("failed to write cycle test file b");

    let output = run_lait_workflow(&a_path, "hello");

    std::fs::remove_file(&a_path).ok();
    std::fs::remove_file(&b_path).ok();

    assert!(
        !output.status.success(),
        "expected a workflow: cycle to be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cycle"), "stderr: {stderr}");
}

#[test]
fn a_registered_workflow_name_can_be_called_as_a_child() {
    let config = ConfigDirectory::new("workflows:\n  shout: ./shout.yml\n");
    std::fs::write(
        config.path().join("shout.yml"),
        "inputs:\n  mark: string\noutput: '. + $inputs.mark'\nsteps:\n  - jq: ascii_upcase\n",
    )
    .unwrap();
    std::fs::write(
        config.path().join("main.yml"),
        "steps:\n  - workflow: shout\n    with: '{mark: \"!\"}'\n",
    )
    .unwrap();

    let output = test_command()
        .current_dir(config.path())
        .args(["run", "main.yml", "hey", "--no-history"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to execute lait run");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "HEY!");
}
