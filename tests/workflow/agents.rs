use super::*;

#[test]
fn step_with_an_agent_renders_its_system_prompt_and_uses_its_output_schema() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"city\":\"Tokyo\"}"},"finish_reason":"stop"}]}"#,
    );
    let agent = AgentMarkdownFile::new(
        r#"---
output_schema:
  type: object
  properties:
    city:
      type: string
  required: [city]
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
  - id: extract
    agent: "{}"
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
fn workflow_agent_nodes_reject_a_self_referential_subagent_before_recursing() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_self","type":"function","function":{"name":"agent__self","arguments":"{\"input\":\"again\"}"}}]},"finish_reason":"tool_calls"}]}"#,
    );
    let agent = AgentMarkdownFile::new(
        "---\nname: self\nmodel: test-model\nsubagents: [self]\n---\nDelegate only when needed.\n",
    );
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\nagents:\n  self: \"{}\"\n",
        server.base_url,
        agent.path.display()
    ));
    let workflow = WorkflowFile::new(&format!(
        r#"
steps:
  - id: invoke
    agent: "{}"
"#,
        agent.path.display()
    ));

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
    server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "expected the self-reference to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cycle"), "stderr: {stderr}");
}

#[test]
fn a_bare_input_placeholder_in_an_agent_body_renders_an_object_input_as_json() {
    let extract_body = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"workflow-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"city\":\"Tokyo\"}"},"finish_reason":"stop"}]}"#;
    let server =
        MockServer::start_sequence(&[("200 OK", extract_body), ("200 OK", CHAT_COMPLETION_BODY)]);
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
    output_schema:
      file: "{}"
  - id: summarize
    agent: "{}"
"#,
        server.base_url,
        extract_schema.path.display(),
        agent.path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "Tokyo has a large population.");
    server.receive_request();
    let summarize_request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    let body: serde_json::Value = serde_json::from_str(&summarize_request.body).unwrap();
    assert_eq!(
        body["messages"][0]["content"], r#"Summarize this: {"city":"Tokyo"}"#,
        "a structured value renders as JSON, not '[object]'"
    );
    assert_eq!(body["messages"][1]["content"], r#"{"city":"Tokyo"}"#);
}

#[test]
fn an_inline_agent_supplies_the_system_prompt_model_and_structured_output() {
    let body = completion_with_content(r#"{"city":"Tokyo"}"#);
    let server = MockServer::start("200 OK", &body);
    let workflow = WorkflowFile::new(&format!(
        r#"
models:
  local:
    - provider:
        base_url: "{}"
      model_id: agent-model
agents:
  extractor:
    model: local
    output_schema:
      type: object
      properties: {{ city: {{ type: string }} }}
      required: [city]
    schema_name: city_fact
    system: "Extract the city from: {{{{ input }}}} ({{{{ inputs.hint }}}})"
inputs:
  hint: {{ type: string, default: none }}
steps:
  - agent: extractor
    output: '.city'
"#,
        server.base_url
    ));

    let output = run_workflow_with(&workflow, &["Tokyo is big"]);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Tokyo");
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], "agent-model");
    assert_eq!(
        body["messages"][0]["content"],
        "Extract the city from: Tokyo is big (none)"
    );
    assert_eq!(body["messages"][1]["content"], "Tokyo is big");
    assert_eq!(body["response_format"]["json_schema"]["name"], "city_fact");
}
