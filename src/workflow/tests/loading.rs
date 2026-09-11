use super::*;

#[test]
fn load_workflow_rejects_a_file_beyond_max_read_bytes() {
    let path = crate::test_support::unique_temp_path("lait-workflow-read-limit", ".yml");
    std::fs::write(&path, vec![b'a'; crate::async_io::MAX_READ_BYTES + 1]).unwrap();

    let error = load_workflow(&path).unwrap_err();
    assert!(
        format!("{error:#}").contains("read limit"),
        "error: {error:#}"
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn workflow_registry_caches_a_loaded_file_by_path() {
    let path = crate::test_support::unique_temp_path("lait-test-workflow-registry", ".yml");
    std::fs::write(
        &path,
        "nodes:\n  noop:\n    type: transform\n    jq: '.'\nsteps:\n  - use: noop\n",
    )
    .unwrap();

    let registry = WorkflowRegistry::new();
    let first = registry.load_path_cancellable(&path, None).await.unwrap();
    let second = registry.load_path_cancellable(&path, None).await.unwrap();

    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "a second load of the same path should hit the cache instead of re-reading/re-parsing the file"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn parses_workflow_with_multiple_steps() {
    let workflow = parse_workflow(
        r#"
name: example
description: summarize then translate
default:
  model: local
nodes:
  summarize:
    type: prompt
    prompt: "summarize: {{ input }}"
  translate:
    type: prompt
    model: cloud
    reasoning_effort: high
    prompt: "translate: {{ input }}"
steps:
  - use: summarize
  - use: translate
"#,
    )
    .expect("workflow should parse");

    assert_eq!(workflow.name.as_deref(), Some("example"));
    assert_eq!(workflow.default.model.as_deref(), Some("local"));
    assert_eq!(workflow.steps.len(), 2);
    assert_eq!(workflow.steps[0].label(), Some("summarize"));
    assert_eq!(workflow.nodes["translate"].settings().model, Some("cloud"));
}

#[test]
fn node_settings_view_preserves_model_and_output_settings() {
    let workflow = parse_workflow(
        r#"
nodes:
  call:
    type: prompt
    model: local
    temperature: 0.4
    mcp: [filesystem]
    jq: '.answer'
    write_file: result.json
    timeout: 10
    retry:
      max_attempts: 2
    prompt: '{{ input }}'
steps:
  - use: call
"#,
    )
    .expect("prompt node with shared settings should parse");

    let node = &workflow.nodes["call"];
    let settings = node.settings();
    assert_eq!(node.kind(), NodeKind::Prompt);
    assert_eq!(settings.model, Some("local"));
    assert_eq!(settings.temperature, Some(0.4));
    assert_eq!(settings.mcp, Some(["filesystem".to_owned()].as_slice()));
    assert_eq!(settings.jq, Some(".answer"));
    assert_eq!(
        settings.write_file.and_then(|path| path.to_str()),
        Some("result.json")
    );
    assert_eq!(settings.timeout, Some(10));
    assert_eq!(settings.retry.and_then(|retry| retry.max_attempts), Some(2));
}

#[test]
fn parses_workflow_with_embedded_models() {
    let workflow = parse_workflow(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
      model_id: local-model
      default_reasoning_effort: medium
  cloud:
    - provider:
        base_url: https://api.example.com/v1
        api_key: secret
      model_id: cloud-model
nodes:
  echo:
    type: prompt
    prompt: "{{ input }}"
  echo_cloud:
    type: prompt
    model: cloud
    prompt: "{{ input }}"
steps:
  - use: echo
  - use: echo_cloud
"#,
    )
    .expect("workflow with embedded models should parse");

    assert_eq!(workflow.models.len(), 2);
    assert!(workflow.models.contains_key("local"));
    assert!(workflow.models.contains_key("cloud"));
}

#[test]
fn rejects_workflow_with_no_steps() {
    assert!(parse_workflow("steps: []\n").is_err());
}
