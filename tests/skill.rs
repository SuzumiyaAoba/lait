mod support;

use support::{AgentMarkdownFile, ConfigDirectory, MockServer, test_command};

const CHAT_COMPLETION_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#;

#[test]
fn agent_run_appends_skill_content_after_the_agents_own_system_prompt() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\nskills:\n  code-review: skill.md\n",
        server.base_url
    ));
    std::fs::write(
        config.path().join("skill.md"),
        "---\nname: code-review\ndescription: reviews a diff for bugs\n---\nLook for off-by-one errors.\n",
    )
    .expect("failed to write test skill file");
    let agent = AgentMarkdownFile::new(
        "---\nname: city-fact\nskills: [code-review]\n---\nYou are a helpful assistant.\nCity: {{ input.city }}\n",
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
        request_json["messages"][0],
        serde_json::json!({
            "role": "system",
            "content": "You are a helpful assistant.\nCity: Tokyo\n\n---\n\n## Skill: code-review\n\nreviews a diff for bugs\n\nLook for off-by-one errors.",
        })
    );
}

#[test]
fn chat_appends_default_skills_with_no_system_prompt_of_its_own() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\n  skills: [code-review]\nskills:\n  code-review: skill.md\n",
        server.base_url
    ));
    std::fs::write(
        config.path().join("skill.md"),
        "---\n---\nLook for off-by-one errors.\n",
    )
    .expect("failed to write test skill file");

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["messages"][0],
        serde_json::json!({
            "role": "system",
            "content": "## Skill: code-review\n\nLook for off-by-one errors.",
        })
    );
}

#[test]
fn agent_run_fails_with_a_clear_error_on_an_unknown_skill_name() {
    let agent =
        AgentMarkdownFile::new("---\nmodel: test-model\nskills: [missing]\n---\nhi {{ input }}\n");

    let output = test_command()
        .arg("agent")
        .arg("run")
        .arg(&agent.path)
        .arg("hi")
        .arg("--no-config")
        .output()
        .expect("failed to execute lait agent run");

    assert!(!output.status.success(), "expected lait agent run to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing"), "stderr: {stderr}");
    assert!(stderr.contains("skills:"), "stderr: {stderr}");
}
