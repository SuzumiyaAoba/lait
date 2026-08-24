mod support;

use support::{
    AgentMarkdownFile, ConfigDirectory, JsonSchemaFile, MockServer, WorkflowFile,
    run_lait_workflow, test_command, without_json_whitespace,
};

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
steps:
  - prompt: "{{{{ input }}}}"
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
steps:
  - prompt: "{{{{ input }}}}"
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
steps:
  - prompt: "{{ input }}"
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
steps:
  - prompt: "{{{{ input }}}}"
    json_schema: "{}"
    schema_name: answer_schema
    jq: ".answer"
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
steps:
  - prompt: "{{{{ input }}}}"
    json_schema: answer
    schema_name: answer_schema
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
steps:
  - prompt: "{{{{ input }}}}"
    json_schema: answer
    schema_name: answer_schema
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
steps:
  - agent: "{}"
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
steps:
  - id: extract
    prompt: "{{{{ input }}}}"
    json_schema: "{}"
  - id: summarize
    agent: "{}"
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
steps:
  - jq: ".name"
"#,
    );

    let output = run_lait_workflow(&workflow.path, r#"{"name":"Alice","age":30}"#);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Alice");
}
