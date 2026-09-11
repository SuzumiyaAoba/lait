use super::*;

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
    type: prompt
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
    type: prompt
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
    type: prompt
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
    type: prompt
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
    type: prompt
    prompt: "{{ input }}"
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
    type: prompt
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
    let missing_path = support::next_temp_path("lait-missing-input-schema", ".json");
    assert!(
        !missing_path.exists(),
        "test schema path unexpectedly exists: {missing_path:?}"
    );
    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  echo:
    type: prompt
    prompt: "{{{{ input }}}}"
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
