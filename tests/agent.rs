mod support;

use support::{AgentMarkdownFile, ConfigDirectory, MockServer, run_lait_agent, test_command};

const CHAT_COMPLETION_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#;

#[test]
fn agent_run_sends_a_rendered_system_prompt_and_the_raw_input_as_the_user_message() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\n",
        server.base_url
    ));
    let agent = AgentMarkdownFile::new(
        "---\nname: city-fact\n---\nYou are a helpful assistant.\nCity: {{ input.city }}\n",
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("agent")
        .arg("run")
        .arg(&agent.path)
        .arg(r#"{"city":"Tokyo"}"#)
        .output()
        .expect("failed to execute lait agent run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait agent run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"],
        serde_json::json!([
            {"role": "system", "content": "You are a helpful assistant.\nCity: Tokyo"},
            {"role": "user", "content": r#"{"city":"Tokyo"}"#},
        ])
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn agent_run_requests_structured_output_when_configured() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"city\":\"Tokyo\"}"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\n",
        server.base_url
    ));
    let agent = AgentMarkdownFile::new(
        r#"---
output_schema:
  schema:
    type: object
    properties:
      city:
        type: string
    required: [city]
    additionalProperties: false
structured_output: true
schema_name: city_fact
---
Extract the city.
{{ input }}
"#,
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("agent")
        .arg("run")
        .arg(&agent.path)
        .arg("Tokyo has a large population.")
        .output()
        .expect("failed to execute lait agent run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait agent run failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["response_format"],
        serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "city_fact",
                "schema": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                    "additionalProperties": false,
                },
                "strict": true,
            },
        })
    );
}

#[test]
fn agent_run_rejects_input_missing_a_field_required_by_the_input_schema() {
    let agent = AgentMarkdownFile::new(
        "---\ninput_schema:\n  schema:\n    type: object\n    required: [city]\n---\n{{ input.city }}\n",
    );

    let output = run_lait_agent(&agent.path, r#"{"other":true}"#);

    assert!(!output.status.success(), "expected lait agent run to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("city"), "stderr: {stderr}");
}

#[test]
fn top_level_agent_rejects_a_self_referential_subagent_before_recursing() {
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

    let output = test_command()
        .current_dir(config.path())
        .args([
            "agent",
            "run",
            agent.path.to_str().unwrap(),
            "hello",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait agent run");
    server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "expected the self-reference to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cycle"), "stderr: {stderr}");
}
