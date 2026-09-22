//! Behavioral coverage for `default.skill_progressive_disclosure: true`
//! (`src/config/types.rs`'s `DefaultSettings::skill_progressive_disclosure`,
//! driven from `src/skill.rs`'s `SkillCache::render_frontmatter`/
//! `skill_body` and `engine/transport.rs`'s `skills_need_tool_loop`): only a
//! skill's `name`/`description` are appended to the system prompt by
//! default; a model that needs the rest calls the matching `skill__<name>`
//! tool. `tests/skill.rs` covers the (default, off) full-body-always
//! behavior and is left untouched by this file.

mod support;

use support::{ConfigDirectory, MockServer, test_command};

fn skill_tool_call_response() -> String {
    r#"{"id":"chatcmpl-tool","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"skill__code-review","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#.to_owned()
}

fn plain_response(content: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-plain","object":"chat.completion","created":0,"model":"test-model","choices":[{{"index":0,"message":{{"role":"assistant","content":"{content}"}},"finish_reason":"stop"}}]}}"#
    )
}

#[test]
fn chat_shows_only_frontmatter_and_reads_the_body_through_a_skill_tool() {
    let server = MockServer::start_sequence(&[
        ("200 OK", &skill_tool_call_response()),
        ("200 OK", &plain_response("done")),
    ]);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\n  skills: [code-review]\n  skill_progressive_disclosure: true\nskills:\n  code-review: skill.md\n",
        server.base_url
    ));
    config.write(
        "skill.md",
        "---\nname: code-review\ndescription: reviews a diff for bugs\n---\nLook for off-by-one errors.\n",
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    let first_request = server.receive_request();
    let second_request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");

    let first_json: serde_json::Value =
        serde_json::from_str(&first_request.body).expect("request body should be valid JSON");
    let system_content = first_json["messages"][0]["content"]
        .as_str()
        .expect("a system message should be present");
    assert!(
        system_content.contains("## Skill: code-review"),
        "system prompt: {system_content}"
    );
    assert!(
        system_content.contains("reviews a diff for bugs"),
        "system prompt: {system_content}"
    );
    assert!(
        !system_content.contains("Look for off-by-one errors"),
        "the body must not be injected up front: {system_content}"
    );
    assert!(
        system_content.contains("skill__code-review"),
        "the frontmatter should point at the tool to read the rest: {system_content}"
    );
    let tool_names: Vec<&str> = first_json["tools"]
        .as_array()
        .expect("tools should be present")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, vec!["skill__code-review"]);

    let second_json: serde_json::Value =
        serde_json::from_str(&second_request.body).expect("request body should be valid JSON");
    let tool_result = second_json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("a tool result message should be present");
    let tool_result_content = tool_result["content"].as_str().unwrap();
    assert!(
        tool_result_content.contains("Look for off-by-one errors"),
        "tool result: {tool_result_content}"
    );
}

#[test]
fn progressive_disclosure_is_off_by_default_and_never_adds_a_skill_tool() {
    // Same skill, no `skill_progressive_disclosure:` set — the model never
    // needs a second request: the body is already in the system prompt (see
    // `tests/skill.rs` for the exact string it produces).
    let server = MockServer::start("200 OK", &plain_response("done"));
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\n  skills: [code-review]\nskills:\n  code-review: skill.md\n",
        server.base_url
    ));
    config.write(
        "skill.md",
        "---\nname: code-review\ndescription: reviews a diff for bugs\n---\nLook for off-by-one errors.\n",
    );

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
    assert!(
        request_json.get("tools").is_none(),
        "no tools should be offered: {request_json:?}"
    );
    let system_content = request_json["messages"][0]["content"].as_str().unwrap();
    assert!(system_content.contains("Look for off-by-one errors"));
}
