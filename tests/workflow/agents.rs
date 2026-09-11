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
    type: agent
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
nodes:
  invoke:
    type: agent
    agent: "{}"
steps:
  - use: invoke
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
    type: prompt
    prompt: "{{{{ input }}}}"
    output_schema: "{}"
  summarize:
    type: agent
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
