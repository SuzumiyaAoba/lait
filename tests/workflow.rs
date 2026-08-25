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

#[test]
fn a_falsy_when_guard_skips_the_step_and_passes_the_input_through_unchanged() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - jq: "."
  - id: guarded
    when: ".flag"
    jq: '"should not run"'
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
steps:
  - id: guarded
    when: ".flag"
    jq: '"ran"'
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
steps:
  - switch:
      cases:
        - when: '.severity == "high"'
          steps:
            - jq: '"escalated"'
        - when: '.severity == "medium"'
          steps:
            - jq: '"replied"'
      else:
        - jq: '"closed"'
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
steps:
  - switch:
      cases:
        - when: '.severity == "high"'
          steps:
            - jq: '"escalated"'
      else:
        - jq: '"closed"'
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
steps:
  - switch:
      cases:
        - when: '.severity == "high"'
          steps:
            - jq: '"escalated"'
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
steps:
  - parallel:
      branches:
        - id: a
          steps:
            - prompt: "{{{{ input }}}}"
        - id: b
          steps:
            - model: cloud
              prompt: "{{{{ input }}}}"
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
steps:
  - parallel:
      branches:
        - id: upper
          steps:
            - jq: 'ascii_upcase'
        - id: length
          steps:
            - jq: 'length'
      join: '{summary: .upper, length: .length}'
  - id: describe
    jq: '.summary + " (" + (.length | tostring) + ")"'
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
steps:
  - parallel:
      branches:
        - id: same
          steps:
            - jq: "."
        - id: same
          steps:
            - jq: "."
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
steps:
  - switch:
      cases:
        - when: '.severity == "high"'
          steps:
            - prompt: "escalate: {{{{ input }}}}"
      else:
        - jq: '"closed"'
  - id: notify
    jq: '. + " (notified)"'
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
