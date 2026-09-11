use super::*;

#[test]
fn transform_only_step_reshapes_input_without_calling_the_model() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  extract_name:
    type: transform
    jq: ".name"
steps:
  - use: extract_name
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"name":"Alice","age":30}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Alice");
}

#[test]
fn a_falsy_when_guard_skips_the_step_and_passes_the_input_through_unchanged() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    type: transform
    jq: "."
  guarded:
    type: transform
    jq: '"should not run"'
steps:
  - use: passthrough
  - id: guarded
    when: ".flag"
    use: guarded
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"flag":false}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        r#"{"flag":false}"#
    );
}

#[test]
fn a_truthy_when_guard_runs_the_step() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  guarded:
    type: transform
    jq: '"ran"'
steps:
  - id: guarded
    when: ".flag"
    use: guarded
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"flag":true}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ran");
}

#[test]
fn switch_runs_the_first_matching_case_and_skips_the_rest() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  escalated:
    type: transform
    jq: '"escalated"'
  replied:
    type: transform
    jq: '"replied"'
  closed:
    type: transform
    jq: '"closed"'
steps:
  - switch:
      cases:
        - when: '.severity == "high"'
          steps:
            - use: escalated
        - when: '.severity == "medium"'
          steps:
            - use: replied
      else:
        - use: closed
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"severity":"medium"}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "replied");
}

#[test]
fn switch_runs_else_when_no_case_matches() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  escalated:
    type: transform
    jq: '"escalated"'
  closed:
    type: transform
    jq: '"closed"'
steps:
  - switch:
      cases:
        - when: '.severity == "high"'
          steps:
            - use: escalated
      else:
        - use: closed
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"severity":"low"}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "closed");
}

#[test]
fn switch_fails_when_no_case_matches_and_there_is_no_else() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  escalated:
    type: transform
    jq: '"escalated"'
steps:
  - switch:
      cases:
        - when: '.severity == "high"'
          steps:
            - use: escalated
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"severity":"low"}"#);

    assert!(
        !output.status.success(),
        "expected an unmatched switch without 'else' to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no case matched"), "stderr: {stderr}");
}

#[test]
fn parallel_runs_every_branch_and_joins_outputs_into_an_id_keyed_object_in_declaration_order() {
    let server_a = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-a","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"response-a"},"finish_reason":"stop"}]}"#,
    );
    let server_b = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-b","object":"chat.completion","created":0,"model":"model-b","choices":[{"index":0,"message":{"role":"assistant","content":"response-b"},"finish_reason":"stop"}]}"#,
    );
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: model-a
  cloud:
    - provider:
        base_url: "{}"
      model_id: model-b
nodes:
  echo_a:
    type: prompt
    prompt: "{{{{ input }}}}"
  echo_b:
    type: prompt
    model: cloud
    prompt: "{{{{ input }}}}"
steps:
  - parallel:
      branches:
        - id: a
          steps:
            - use: echo_a
        - id: b
          steps:
            - use: echo_b
"#,
        server_a.base_url, server_b.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server_a.receive_request();
    server_b.receive_request();
    server_a.finish();
    server_b.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        r#"{"a":"response-a","b":"response-b"}"#
    );
}

#[test]
fn parallel_join_filter_combines_the_id_keyed_object_into_the_next_input() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  upper:
    type: transform
    jq: 'ascii_upcase'
  length_of:
    type: transform
    jq: 'length'
  describe:
    type: transform
    jq: '.summary + " (" + (.length | tostring) + ")"'
steps:
  - parallel:
      branches:
        - id: upper
          steps:
            - use: upper
        - id: length
          steps:
            - use: length_of
      join: '{summary: .upper, length: .length}'
  - id: describe
    use: describe
"#,
    );

    let output = run_lait_workflow(&workflow.path, "\"hi\"");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "HI (2)");
}

#[test]
fn parallel_fails_when_a_branch_id_is_duplicated() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    type: transform
    jq: "."
steps:
  - parallel:
      branches:
        - id: same
          steps:
            - use: passthrough
        - id: same
          steps:
            - use: passthrough
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(
        !output.status.success(),
        "expected duplicate branch ids to be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("duplicate id"), "stderr: {stderr}");
}

#[test]
fn switch_case_can_call_the_model_and_continues_the_outer_steps_afterward() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"\"escalation memo\""},"finish_reason":"stop"}]}"#,
    );
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  escalate:
    type: prompt
    prompt: "escalate: {{{{ json input }}}}"
  closed:
    type: transform
    jq: '"closed"'
  notify:
    type: transform
    jq: '. + " (notified)"'
steps:
  - switch:
      cases:
        - when: '.severity == "high"'
          steps:
            - use: escalate
      else:
        - use: closed
  - id: notify
    use: notify
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, r#"{"severity":"high"}"#);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""content":"escalate:{\"severity\":\"high\"}""#),
        "request body: {body}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "escalation memo (notified)"
    );
}
