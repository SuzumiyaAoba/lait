use super::*;

#[test]
fn an_env_var_placeholder_expands_in_a_workflow_models_api_key() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
        api_key: "${{LAIT_TEST_ENV_API_KEY}}"
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

    let output = test_command()
        .env("LAIT_TEST_ENV_API_KEY", "secret-from-env")
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .output()
        .expect("failed to execute lait run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer secret-from-env"),
        "headers: {}",
        request.headers
    );
}

#[test]
fn an_unset_env_var_placeholder_fails_with_a_clear_error() {
    let workflow = WorkflowFile::new(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: http://127.0.0.1:65535/v1
        api_key: "${LAIT_TEST_ENV_DEFINITELY_UNSET}"
      model_id: workflow-model
nodes:
  echo:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: echo
"#,
    );

    let output = test_command()
        .env_remove("LAIT_TEST_ENV_DEFINITELY_UNSET")
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .output()
        .expect("failed to execute lait run");

    assert!(
        !output.status.success(),
        "expected a missing env var placeholder to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("LAIT_TEST_ENV_DEFINITELY_UNSET"),
        "stderr: {stderr}"
    );
}
