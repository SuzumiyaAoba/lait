use super::*;

#[test]
fn resolves_model_and_base_url_from_models_embedded_in_the_workflow_file() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
        api_key: workflow-key
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
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer workflow-key")
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"workflow-model""#),
        "request body: {body}"
    );
}

#[test]
fn run_emits_json_with_the_same_shape_as_chat() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\n",
        server.base_url
    ));
    let workflow = WorkflowFile::new(
        "nodes:\n  echo:\n    type: prompt\n    prompt: \"{{ input }}\"\nsteps:\n  - use: echo\n",
    );

    let output = test_command()
        .current_dir(config.path())
        .args(["run", workflow.path.to_str().unwrap(), "hello", "--json"])
        .output()
        .expect("failed to execute lait run");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(
        json,
        serde_json::json!({
            "content": "mock response",
            "reasoning": null,
            "usage": null,
        })
    );
}

#[test]
fn workflow_level_alias_takes_precedence_over_a_config_file_alias_of_the_same_name() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let config = ConfigDirectory::new(
        "models:\n  shared:\n    - provider:\n        base_url: http://127.0.0.1:65535/v1\n      model_id: config-model\n",
    );
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: shared
models:
  shared:
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

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .output()
        .expect("failed to execute lait run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"workflow-model""#),
        "expected the workflow's own alias to win over the config file's, request body: {body}"
    );
}

#[test]
fn step_falls_back_to_the_config_file_default_model_when_workflow_omits_one() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: config-model\n",
        server.base_url
    ));
    let workflow = WorkflowFile::new(
        r#"
nodes:
  echo:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: echo
"#,
    );

    let output = test_command()
        .current_dir(config.path())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "hello",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"config-model""#),
        "request body: {body}"
    );
}

#[test]
fn a_step_overrides_the_workflow_default_temperature_top_p_and_max_tokens() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
  temperature: 0.2
  top_p: 0.5
  max_tokens: 64
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  echo:
    type: prompt
    temperature: 0.9
    top_p: 0.95
    max_tokens: 512
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
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""temperature":0.9"#),
        "request body: {body}"
    );
    assert!(body.contains(r#""top_p":0.95"#), "request body: {body}");
    assert!(
        body.contains(r#""max_completion_tokens":512"#),
        "request body: {body}"
    );
}

#[test]
fn a_step_falls_back_to_the_workflow_default_temperature_when_it_has_none_of_its_own() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
  temperature: 0.3
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
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""temperature":0.3"#),
        "request body: {body}"
    );
}

#[test]
fn step_falls_back_to_a_config_file_alias_when_not_defined_in_the_workflow() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let config = ConfigDirectory::new(&format!(
        "models:\n  from-config:\n    - provider:\n        base_url: \"{}\"\n      model_id: config-model\n",
        server.base_url
    ));
    let workflow = WorkflowFile::new(
        r#"
default:
  model: from-config
nodes:
  echo:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: echo
"#,
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .output()
        .expect("failed to execute lait run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"config-model""#),
        "request body: {body}"
    );
}

#[test]
fn a_node_overrides_the_workflow_default_skills() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\nskills:\n  from-default: from-default.md\n  from-node: from-node.md\n",
        server.base_url
    ));
    std::fs::write(
        config.path().join("from-default.md"),
        "---\n---\nworkflow default skill body\n",
    )
    .expect("failed to write test skill file");
    std::fs::write(
        config.path().join("from-node.md"),
        "---\n---\nnode-level skill body\n",
    )
    .expect("failed to write test skill file");
    let workflow = WorkflowFile::new(
        r#"
default:
  model: workflow-model
  skills: [from-default]
nodes:
  echo:
    type: prompt
    prompt: "{{ input }}"
    skills: [from-node]
steps:
  - use: echo
"#,
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow.path)
        .arg("hello")
        .output()
        .expect("failed to execute lait run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"][0],
        serde_json::json!({
            "role": "system",
            "content": "## Skill: from-node\n\nnode-level skill body",
        })
    );
}
