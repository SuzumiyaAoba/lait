use super::*;

#[test]
fn a_workflow_step_runs_a_sub_workflow_and_uses_its_output() {
    let sub = WorkflowFile::new(
        r#"
nodes:
  add_one:
    type: transform
    jq: '. + 1'
steps:
  - use: add_one
"#,
    );
    let sub_name = sub.path.file_name().unwrap().to_str().unwrap();
    let parent = WorkflowFile::new(&format!(
        r#"
nodes:
  sub:
    type: workflow
    workflow: {sub_name}
    jq: '. * 2'
steps:
  - use: sub
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
nodes:
  echo:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: echo
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
nodes:
  sub:
    type: workflow
    workflow: {sub_name}
steps:
  - use: sub
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
nodes:
  echo:
    type: prompt
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
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
nodes:
  sub:
    type: workflow
    workflow: {sub_name}
steps:
  - use: sub
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
nodes:
  inner:
    type: transform
    jq: '$steps.outer'
steps:
  - id: inner
    use: inner
"#,
    );
    let sub_name = sub.path.file_name().unwrap().to_str().unwrap();
    let parent = WorkflowFile::new(&format!(
        r#"
nodes:
  outer:
    type: transform
    jq: '{{ from_outer: true }}'
  sub:
    type: workflow
    workflow: {sub_name}
  read_inner:
    type: transform
    jq: '$steps.inner'
steps:
  - id: outer
    use: outer
  - use: sub
  - use: read_inner
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
    let a_path = support::next_temp_path("lait-test-cycle-a", ".yml");
    let b_path = support::next_temp_path("lait-test-cycle-b", ".yml");

    std::fs::write(
        &a_path,
        format!(
            "nodes:\n  sub:\n    workflow: {}\nsteps:\n  - use: sub\n",
            b_path.file_name().unwrap().to_str().unwrap()
        ),
    )
    .expect("failed to write cycle test file a");
    std::fs::write(
        &b_path,
        format!(
            "nodes:\n  sub:\n    workflow: {}\nsteps:\n  - use: sub\n",
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
