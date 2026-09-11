use super::*;

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
    type: prompt
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
    type: prompt
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
    type: agent
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
