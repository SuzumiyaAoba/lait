use super::*;

#[test]
fn a_prompt_node_sends_its_rendered_system_prompt_ahead_of_the_user_prompt() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
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
  echo:
    type: prompt
    system_prompt: "Reply in {{{{ input }}}}."
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "French");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"],
        serde_json::json!([
            {"role": "system", "content": "Reply in French."},
            {"role": "user", "content": "French"},
        ])
    );
}

#[test]
fn a_node_overrides_the_workflow_default_system_prompt() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
  system_prompt: from default
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  echo:
    type: prompt
    system_prompt: from node
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"][0],
        serde_json::json!({"role": "system", "content": "from node"})
    );
}

#[test]
fn a_prompt_node_falls_back_to_the_workflow_default_system_prompt() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
  system_prompt: from default
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  echo:
    type: prompt
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"][0],
        serde_json::json!({"role": "system", "content": "from default"})
    );
}

#[test]
fn a_node_with_no_prompt_sends_the_current_input_unchanged_as_the_user_message() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
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
  echo:
    type: prompt
    system_prompt: "Reply in French."
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, r#"{"not":"json-safe for a bare template"}"#);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"],
        serde_json::json!([
            {"role": "system", "content": "Reply in French."},
            {"role": "user", "content": r#"{"not":"json-safe for a bare template"}"#},
        ])
    );
}
