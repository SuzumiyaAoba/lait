mod support;

use std::time::Duration;

use support::{
    AgentMarkdownFile, ConfigDirectory, JsonSchemaFile, MockServer, WorkflowFile,
    run_lait_workflow, test_command, without_json_whitespace,
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
    output_schema: "{}"
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
    output_schema: answer
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
    output_schema: answer
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
steps:
  - prompt: "{{{{ json input }}}}"
    input_schema: city
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
steps:
  - jq: "."
    input_schema: city
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
steps:
  - prompt: "{{{{ json input }}}}"
    input_schema: "{}"
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
steps:
  - jq: "."
    input_schema: "{}"
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
    output_schema: "{}"
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
            - prompt: "escalate: {{{{ json input }}}}"
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

#[test]
fn loop_while_reruns_steps_until_the_pre_check_condition_goes_false() {
    let workflow = WorkflowFile::new(
        r#"
steps:
  - id: count-up
    loop:
      while: '.n < 3'
      max_iterations: 10
      steps:
        - jq: '{n: (.n + 1)}'
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
steps:
  - loop:
      while: '.n < 0'
      max_iterations: 10
      steps:
        - jq: '"should not run"'
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
steps:
  - loop:
      while: 'true'
      max_iterations: 3
      steps:
        - jq: '.'
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
steps:
  - id: retry
    loop:
      until: '.n >= 3'
      max_iterations: 10
      steps:
        - jq: '{n: (.n + 1)}'
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
steps:
  - loop:
      until: 'false'
      max_iterations: 3
      steps:
        - jq: '.'
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
steps:
  - for_each:
      items: '.names'
      steps:
        - prompt: "summarize: {{{{ input }}}}"
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
steps:
  - id: double
    for_each:
      items: '.items'
      steps:
        - jq: '. * 2'
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
steps:
  - for_each:
      items: '.items'
      steps:
        - jq: '. * 2'
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
steps:
  - for_each:
      items: '.items'
      steps:
        - jq: '"should not run"'
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
steps:
  - for_each:
      items: '.count'
      steps:
        - jq: '.'
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
steps:
  - id: call
    prompt: "{{{{ input }}}}"
    stop: true
  - id: never
    jq: '"should not run"'
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
steps:
  - loop:
      until: '.n >= 10'
      max_iterations: 5
      steps:
        - jq: '.n += 1'
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
steps:
  - for_each:
      items: '.items'
      steps:
        - when: '. == 2'
          break: true
        - jq: '.'
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
steps:
  - loop:
      until: '.n >= 10'
      max_iterations: 5
      steps:
        - jq: '.n += 1'
        - when: '.n == 2'
          stop: true
  - jq: '"should not run"'
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
steps:
  - id: call
    prompt: "{{{{ input }}}}"
    retry:
      max_attempts: 2
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
steps:
  - id: call
    prompt: "{{{{ input }}}}"
    retry:
      max_attempts: 2
    on_error:
      steps:
        - jq: '.input'
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
steps:
  - prompt: "{{{{ input }}}}"
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
steps:
  - id: call
    prompt: "{{{{ input }}}}"
    timeout: 1
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
steps:
  - id: extract
    prompt: "{{{{ input }}}}"
    output_schema: city
    schema_name: city
  - prompt: "city was {{{{ steps.extract.city }}}}"
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
steps:
  - id: check
    jq: '{ ok: true }'
  - jq: '$steps.check.ok'
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
steps:
  - parallel:
      branches:
        - steps:
            - id: inner
              jq: '"branch value"'
  - jq: '$steps.inner'
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
steps:
  - for_each:
      items: '.items'
      max_concurrency: 3
      steps:
        - jq: '. * 10'
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
steps:
  - for_each:
      items: '.items'
      max_concurrency: 3
      steps:
        - prompt: "{{{{ input }}}}"
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
