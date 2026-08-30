mod support;

use std::time::Duration;

use support::{
    AgentMarkdownFile, ConfigDirectory, JsonSchemaFile, MINIMAL_PNG_BYTES, MockServer,
    WorkflowFile, run_lait_workflow, test_command, without_json_whitespace,
};

const SERVER_ERROR_BODY: &str = r#"{"error":{"message":"mock failure","type":"server_error"}}"#;

const CHAT_COMPLETION_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#;

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

#[test]
fn step_requests_structured_output_and_extracts_a_field_with_jq() {
    let schema = JsonSchemaFile::new(
        r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#,
    );
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"answer\":\"42\"}"},"finish_reason":"stop"}]}"#,
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
  answer:
    prompt: "{{{{ input }}}}"
    output_schema: "{}"
    schema_name: answer_schema
    jq: ".answer"
steps:
  - use: answer
"#,
        server.base_url,
        schema.path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["response_format"],
        serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "answer_schema",
                "schema": {
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                    "additionalProperties": false,
                },
                "strict": true,
            },
        })
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn step_requests_structured_output_using_an_inline_schema_from_the_workflows_json_schemas_map() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"answer\":\"42\"}"},"finish_reason":"stop"}]}"#,
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
json_schemas:
  answer:
    schema:
      type: object
      properties:
        answer:
          type: string
      required: [answer]
      additionalProperties: false
nodes:
  answer:
    prompt: "{{{{ input }}}}"
    output_schema: answer
    schema_name: answer_schema
steps:
  - use: answer
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
        request_json["response_format"],
        serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "answer_schema",
                "schema": {
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                    "additionalProperties": false,
                },
                "strict": true,
            },
        })
    );
}

#[test]
fn step_requests_structured_output_using_a_file_path_schema_from_the_workflows_json_schemas_map() {
    let schema = JsonSchemaFile::new(
        r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#,
    );
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"answer\":\"42\"}"},"finish_reason":"stop"}]}"#,
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
json_schemas:
  answer:
    file_path: "{}"
nodes:
  answer:
    prompt: "{{{{ input }}}}"
    output_schema: answer
    schema_name: answer_schema
steps:
  - use: answer
"#,
        server.base_url,
        schema.path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["response_format"],
        serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "answer_schema",
                "schema": {
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                    "additionalProperties": false,
                },
                "strict": true,
            },
        })
    );
}

#[test]
fn step_input_schema_allows_a_call_when_input_has_every_required_field() {
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
json_schemas:
  city:
    schema:
      type: object
      required: [city]
nodes:
  echo:
    prompt: "{{{{ json input }}}}"
    input_schema: city
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, r#"{"city":"Tokyo"}"#);
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
}

#[test]
fn step_input_schema_rejects_input_missing_a_required_field() {
    let workflow = WorkflowFile::new(
        r#"
json_schemas:
  city:
    schema:
      type: object
      required: [city]
nodes:
  echo:
    jq: "."
    input_schema: city
steps:
  - use: echo
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"other":true}"#);

    assert!(
        !output.status.success(),
        "expected the step to reject input missing 'city'"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("city"), "stderr: {stderr}");
}

#[test]
fn step_input_schema_resolves_a_direct_file_path_when_no_json_schemas_map_entry_matches() {
    let schema = JsonSchemaFile::new(r#"{"type":"object","required":["city"]}"#);
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
    prompt: "{{{{ json input }}}}"
    input_schema: "{}"
steps:
  - use: echo
"#,
        server.base_url,
        schema.path.display()
    ));

    let output = run_lait_workflow(&workflow.path, r#"{"city":"Tokyo"}"#);
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
}

#[test]
fn step_input_schema_reports_a_missing_schema_file_with_path_context() {
    let missing_path = std::env::temp_dir().join(format!(
        "lait-missing-input-schema-{}.json",
        std::process::id()
    ));
    assert!(
        !missing_path.exists(),
        "test schema path unexpectedly exists: {missing_path:?}"
    );
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  echo:
    jq: "."
    input_schema: "{}"
steps:
  - use: echo
"#,
        missing_path.display()
    ));

    let output = run_lait_workflow(&workflow.path, r#"{"city":"Tokyo"}"#);

    assert!(
        !output.status.success(),
        "expected the step to fail on a missing input_schema file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to read JSON schema file"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(missing_path.to_string_lossy().as_ref()),
        "stderr: {stderr}"
    );
}

#[test]
fn step_with_an_agent_renders_its_system_prompt_and_uses_its_output_schema() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"city\":\"Tokyo\"}"},"finish_reason":"stop"}]}"#,
    );
    let agent = AgentMarkdownFile::new(
        r#"---
output_schema:
  schema:
    type: object
    properties:
      city:
        type: string
    required: [city]
structured_output: true
schema_name: city_fact
---
Extract the city from: {{ input }}
"#,
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
  extract:
    agent: "{}"
steps:
  - use: extract
"#,
        server.base_url,
        agent.path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "Tokyo has a large population.");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"],
        serde_json::json!([
            {"role": "system", "content": "Extract the city from: Tokyo has a large population."},
            {"role": "user", "content": "Tokyo has a large population."},
        ])
    );
    assert_eq!(
        request_json["response_format"]["json_schema"]["name"],
        "city_fact"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"city":"Tokyo"}"#
    );
}

#[test]
fn a_bare_input_placeholder_in_an_agent_body_rejects_an_object_input_from_a_previous_step() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"city\":\"Tokyo\"}"},"finish_reason":"stop"}]}"#,
    );
    let extract_schema = JsonSchemaFile::new(
        r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}"#,
    );
    let agent = AgentMarkdownFile::new("---\n---\nSummarize this: {{ input }}\n");
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
  extract:
    prompt: "{{{{ input }}}}"
    output_schema: "{}"
  summarize:
    agent: "{}"
steps:
  - id: extract
    use: extract
  - id: summarize
    use: summarize
"#,
        server.base_url,
        extract_schema.path.display(),
        agent.path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "Tokyo has a large population.");
    server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "expected the second step to fail rather than send '[object]' to the model"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("json input"), "stderr: {stderr}");
}

#[test]
fn transform_only_step_reshapes_input_without_calling_the_model() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  extract_name:
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
    jq: "."
  guarded:
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
    jq: '"escalated"'
  replied:
    jq: '"replied"'
  closed:
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
    jq: '"escalated"'
  closed:
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
    prompt: "{{{{ input }}}}"
  echo_b:
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
    jq: 'ascii_upcase'
  length_of:
    jq: 'length'
  describe:
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
    prompt: "escalate: {{{{ json input }}}}"
  closed:
    jq: '"closed"'
  notify:
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

#[test]
fn loop_while_reruns_steps_until_the_pre_check_condition_goes_false() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  bump:
    jq: '{n: (.n + 1)}'
steps:
  - id: count-up
    loop:
      while: '.n < 3'
      max_iterations: 10
      steps:
        - use: bump
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        r#"{"n":3}"#
    );
}

#[test]
fn loop_while_runs_zero_times_when_the_condition_starts_false() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  never:
    jq: '"should not run"'
steps:
  - loop:
      while: '.n < 0'
      max_iterations: 10
      steps:
        - use: never
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":5}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        r#"{"n":5}"#
    );
}

#[test]
fn loop_while_fails_when_max_iterations_is_reached_without_the_condition_going_false() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    jq: '.'
steps:
  - loop:
      while: 'true'
      max_iterations: 3
      steps:
        - use: passthrough
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(
        !output.status.success(),
        "expected an unsatisfied while-loop to fail once max_iterations is reached"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("max_iterations"), "stderr: {stderr}");
}

#[test]
fn loop_until_runs_at_least_once_and_stops_once_the_post_check_condition_is_true() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  bump:
    jq: '{n: (.n + 1)}'
steps:
  - id: retry
    loop:
      until: '.n >= 3'
      max_iterations: 10
      steps:
        - use: bump
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        r#"{"n":3}"#
    );
}

#[test]
fn loop_until_fails_when_max_iterations_is_reached_without_the_condition_going_true() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    jq: '.'
steps:
  - loop:
      until: 'false'
      max_iterations: 3
      steps:
        - use: passthrough
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(
        !output.status.success(),
        "expected an unsatisfied until-loop to fail once max_iterations is reached"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("max_iterations"), "stderr: {stderr}");
}

#[test]
fn for_each_passes_a_string_item_to_a_prompt_unquoted() {
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
  summarize:
    prompt: "summarize: {{{{ input }}}}"
steps:
  - for_each:
      items: '.names'
      steps:
        - use: summarize
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, r#"{"names":["Alice"]}"#);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""content":"summarize:Alice""#),
        "expected the string item to be passed through unquoted, request body: {body}"
    );
}

#[test]
fn for_each_runs_steps_per_item_and_collects_results_in_order() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  doubler:
    jq: '. * 2'
steps:
  - id: double
    for_each:
      items: '.items'
      steps:
        - use: doubler
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[1,2,3]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        without_json_whitespace(&String::from_utf8_lossy(&output.stdout)),
        "[2,4,6]"
    );
}

#[test]
fn for_each_join_filter_combines_the_collected_array_into_the_next_input() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  doubler:
    jq: '. * 2'
steps:
  - for_each:
      items: '.items'
      steps:
        - use: doubler
      join: 'add'
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[1,2,3]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "12");
}

#[test]
fn for_each_runs_zero_times_on_an_empty_array_and_yields_an_empty_result() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  never:
    jq: '"should not run"'
steps:
  - for_each:
      items: '.items'
      steps:
        - use: never
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[]");
}

#[test]
fn for_each_fails_when_items_does_not_produce_a_json_array() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    jq: '.'
steps:
  - for_each:
      items: '.count'
      steps:
        - use: passthrough
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"count":3}"#);

    assert!(
        !output.status.success(),
        "expected a non-array 'items' output to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must produce a JSON array"),
        "stderr: {stderr}"
    );
}

#[test]
fn stop_ends_the_workflow_with_the_current_steps_output_and_skips_later_steps() {
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
  call:
    prompt: "{{{{ input }}}}"
  never:
    jq: '"should not run"'
steps:
  - id: call
    use: call
    stop: true
  - id: never
    use: never
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn break_stops_a_loop_before_its_until_condition_or_max_iterations() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  bump:
    jq: '.n += 1'
steps:
  - loop:
      until: '.n >= 10'
      max_iterations: 5
      steps:
        - use: bump
        - when: '.n == 2'
          break: true
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), r#"{"n":2}"#);
}

#[test]
fn break_stops_a_for_each_early_and_joins_only_the_items_processed_so_far() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  passthrough:
    jq: '.'
steps:
  - for_each:
      items: '.items'
      steps:
        - when: '. == 2'
          break: true
        - use: passthrough
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"items":[1,2,3]}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[1,2]");
}

#[test]
fn stop_inside_a_loop_ends_the_whole_workflow_not_just_the_loop() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  bump:
    jq: '.n += 1'
  never:
    jq: '"should not run"'
steps:
  - loop:
      until: '.n >= 10'
      max_iterations: 5
      steps:
        - use: bump
        - when: '.n == 2'
          stop: true
  - use: never
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"n":0}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), r#"{"n":2}"#);
}

#[test]
fn retry_succeeds_after_a_transient_failure() {
    let server = MockServer::start_sequence(&[
        ("500 Internal Server Error", SERVER_ERROR_BODY),
        ("200 OK", CHAT_COMPLETION_BODY),
    ]);
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
  call:
    prompt: "{{{{ input }}}}"
    retry:
      max_attempts: 2
steps:
  - id: call
    use: call
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn retry_exhausts_all_attempts_and_runs_on_error() {
    let server = MockServer::start_sequence(&[
        ("500 Internal Server Error", SERVER_ERROR_BODY),
        ("500 Internal Server Error", SERVER_ERROR_BODY),
    ]);
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
  call:
    prompt: "{{{{ input }}}}"
    retry:
      max_attempts: 2
  recover:
    jq: '.input'
steps:
  - id: call
    use: call
    on_error:
      steps:
        - use: recover
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(
        output.status.success(),
        "'on_error' should recover the workflow: {output:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
}

#[test]
fn a_step_without_on_error_fails_the_workflow_after_exhausting_retries() {
    let server = MockServer::start_sequence(&[("500 Internal Server Error", SERVER_ERROR_BODY)]);
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
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "expected the workflow to fail without a 'retry'/'on_error'"
    );
}

#[test]
fn timeout_fails_a_step_whose_attempt_exceeds_the_limit() {
    let server = MockServer::start_delayed(Duration::from_secs(2), "200 OK", CHAT_COMPLETION_BODY);
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
  call:
    prompt: "{{{{ input }}}}"
    timeout: 1
steps:
  - id: call
    use: call
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    // Not calling server.finish(): the client aborts before the delayed
    // response is written, which would otherwise make the server thread's
    // write fail; only the CLI's own timeout behavior is under test here.

    assert!(
        !output.status.success(),
        "expected a slow attempt past 'timeout' to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out"), "stderr: {stderr}");
}

#[test]
fn a_later_step_can_reference_an_earlier_named_steps_output_in_its_prompt() {
    let server = MockServer::start_sequence(&[
        (
            "200 OK",
            r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"city\":\"Tokyo\"}"},"finish_reason":"stop"}]}"#,
        ),
        ("200 OK", CHAT_COMPLETION_BODY),
    ]);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
json_schemas:
  city:
    schema:
      type: object
      properties:
        city: {{ type: string }}
      required: [city]
      additionalProperties: false
nodes:
  extract:
    prompt: "{{{{ input }}}}"
    output_schema: city
    schema_name: city
  greet:
    prompt: "city was {{{{ steps.extract.city }}}}"
steps:
  - id: extract
    use: extract
  - use: greet
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    let second_request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert!(
        second_request.body.contains("city was Tokyo"),
        "request body: {}",
        second_request.body
    );
}

#[test]
fn a_jq_filter_can_reference_an_earlier_named_steps_output_via_dollar_steps() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  check:
    jq: '{ ok: true }'
  read_check:
    jq: '$steps.check.ok'
steps:
  - id: check
    use: check
  - use: read_check
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
}

#[test]
fn a_named_step_output_recorded_inside_a_parallel_branch_does_not_leak_outside_it() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  inner:
    jq: '"branch value"'
  read_inner:
    jq: '$steps.inner'
steps:
  - parallel:
      branches:
        - steps:
            - id: inner
              use: inner
  - use: read_inner
"#,
    );

    let output = run_lait_workflow(&workflow.path, "null");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "null",
        "expected the parallel-branch-local step id 'inner' not to be visible outside the branch"
    );
}

#[test]
fn concurrent_for_each_preserves_item_order_in_its_results_regardless_of_completion_order() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  times10:
    jq: '. * 10'
steps:
  - for_each:
      items: '.items'
      max_concurrency: 3
      steps:
        - use: times10
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
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  echo:
    prompt: "{{{{ input }}}}"
steps:
  - for_each:
      items: '.items'
      max_concurrency: 3
      steps:
        - use: echo
      join: 'length'
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
steps:
  - for_each:
      items: '.items'
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
steps:
  - for_each:
      items: '.items'
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
fn a_workflow_step_runs_a_sub_workflow_and_uses_its_output() {
    let sub = WorkflowFile::new(
        r#"
nodes:
  add_one:
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
    jq: '{{ from_outer: true }}'
  sub:
    workflow: {sub_name}
  read_inner:
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

#[test]
fn write_file_writes_the_steps_output_without_changing_what_flows_downstream() {
    let server = MockServer::start_sequence(&[
        ("200 OK", CHAT_COMPLETION_BODY),
        ("200 OK", CHAT_COMPLETION_BODY),
    ]);
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let output_path = std::env::temp_dir().join(format!("lait-test-write-file-{unique}.txt"));
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
  written:
    prompt: "{{{{ input }}}}"
    write_file: "{}"
  echo:
    prompt: "echo: {{{{ steps.written }}}}"
steps:
  - id: written
    use: written
  - use: echo
"#,
        server.base_url,
        output_path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.receive_request();
    server.finish();

    let written = std::fs::read_to_string(&output_path);
    std::fs::remove_file(&output_path).ok();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        written.expect("write_file should have created the output file"),
        "mock response"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response",
        "the next step should still see write_file's own (unmodified) output"
    );
}

#[test]
fn a_default_retry_recovers_a_step_without_its_own_retry() {
    let server = MockServer::start_sequence(&[
        ("500 Internal Server Error", SERVER_ERROR_BODY),
        ("200 OK", CHAT_COMPLETION_BODY),
    ]);
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
  retry:
    max_attempts: 2
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  echo:
    prompt: "{{{{ input }}}}"
steps:
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(
        output.status.success(),
        "the workflow's 'default.retry' should have recovered the step: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

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

#[test]
fn a_command_node_pipes_the_current_input_to_stdin_and_its_stdout_becomes_the_next_input() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  upper:
    command: ["tr", "a-z", "A-Z"]
steps:
  - use: upper
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello world");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "HELLO WORLD"
    );
}

#[test]
fn a_command_nodes_arguments_are_rendered_as_templates() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  greet:
    command: ["echo", "hello, {{ input }}"]
steps:
  - use: greet
"#,
    );

    let output = run_lait_workflow(&workflow.path, "world");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello, world"
    );
}

#[test]
fn a_command_nodes_output_flows_into_a_jq_filter_and_the_next_step() {
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
  count:
    command: ["wc", "-l"]
    jq: 'tonumber | {{lines: .}}'
  echo:
    prompt: "{{{{ json input }}}}"
steps:
  - id: count
    use: count
  - use: echo
"#,
        server.base_url
    ));

    let output = run_lait_workflow(&workflow.path, "a\nb\nc\n");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(request_json["messages"][0]["content"], r#"{"lines":3}"#);
}

#[test]
fn a_commands_nonzero_exit_fails_the_step_with_stderr_in_the_error() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  fail:
    command: ["sh", "-c", "echo boom >&2; exit 3"]
steps:
  - use: fail
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(!output.status.success(), "expected the step to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("boom"), "stderr: {stderr}");
}

#[test]
fn a_nonzero_command_exit_can_be_caught_by_on_error() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  fail:
    command: ["sh", "-c", "exit 1"]
  recover:
    jq: '"recovered"'
steps:
  - use: fail
    on_error:
      steps:
        - use: recover
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(
        output.status.success(),
        "on_error should have recovered the failing command: {output:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "recovered");
}

#[test]
fn an_empty_command_list_is_a_clear_lint_error() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  n:
    command: []
steps:
  - use: n
"#,
    );

    let output = run_lait_workflow(&workflow.path, "hello");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("empty 'command'"), "stderr: {stderr}");
}

#[test]
fn a_prompt_node_attaches_files_as_a_fenced_block_after_the_rendered_prompt() {
    let dir = ConfigDirectory::empty();
    let file_path = dir.path().join("notes.txt");
    std::fs::write(&file_path, "line one\nline two\n").unwrap();

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
    prompt: "summarize: {{{{ input }}}}"
    files: ["{}"]
steps:
  - use: echo
"#,
        server.base_url,
        file_path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert!(request.body.contains("summarize: hello"));
    assert!(request.body.contains(&file_path.display().to_string()));
    assert!(request.body.contains("line one"));
    assert!(request.body.contains("line two"));
}

#[test]
fn a_prompt_node_attaches_images_as_image_url_content_parts() {
    let dir = ConfigDirectory::empty();
    let image_path = dir.path().join("photo.png");
    std::fs::write(&image_path, MINIMAL_PNG_BYTES).unwrap();

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
  describe:
    prompt: "what is this? {{{{ input }}}}"
    images: ["{}"]
steps:
  - use: describe
"#,
        server.base_url,
        image_path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    let content = request_json["messages"][0]["content"]
        .as_array()
        .expect("content should be an array when an image is attached");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"));
}

#[test]
fn an_agent_node_attaches_files_and_images_alongside_its_current_input() {
    let dir = ConfigDirectory::empty();
    let file_path = dir.path().join("notes.txt");
    std::fs::write(&file_path, "note content").unwrap();
    let image_path = dir.path().join("photo.png");
    std::fs::write(&image_path, MINIMAL_PNG_BYTES).unwrap();

    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let agent = AgentMarkdownFile::new("---\n---\nDescribe: {{ input }}\n");
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
  describe:
    agent: "{}"
    files: ["{}"]
    images: ["{}"]
steps:
  - use: describe
"#,
        server.base_url,
        agent.path.display(),
        file_path.display(),
        image_path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "a photo");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    let content = request_json["messages"][1]["content"]
        .as_array()
        .expect("content should be an array when an image is attached");
    assert_eq!(content[0]["type"], "text");
    assert!(content[0]["text"].as_str().unwrap().contains("a photo"));
    assert!(
        content[0]["text"]
            .as_str()
            .unwrap()
            .contains("note content")
    );
    assert_eq!(content[1]["type"], "image_url");
}
