use super::*;

#[test]
fn run_var_overrides_a_prompt_templates_vars_placeholder() {
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
  greet:
    type: prompt
    prompt: "{{{{ input }}}} in {{{{ vars.lang }}}}"
steps:
  - use: greet
"#,
        server.base_url
    ));

    let output = test_command()
        .args([
            "run",
            workflow.path.to_str().expect("workflow path is utf-8"),
            "hello",
            "--var",
            "lang=英語",
        ])
        .output()
        .expect("failed to execute lait run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert!(
        request.body.contains("hello in 英語"),
        "request body: {}",
        request.body
    );
}

#[test]
fn a_later_var_wins_when_the_same_key_is_passed_twice() {
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
  greet:
    type: prompt
    prompt: "{{{{ vars.lang }}}}"
steps:
  - use: greet
"#,
        server.base_url
    ));

    let output = test_command()
        .args([
            "run",
            workflow.path.to_str().expect("workflow path is utf-8"),
            "hello",
            "--var",
            "lang=ja",
            "--var",
            "lang=en",
        ])
        .output()
        .expect("failed to execute lait run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert!(
        request.body.contains("\"en\""),
        "request body: {}",
        request.body
    );
    assert!(
        !request.body.contains("\"ja\""),
        "request body: {}",
        request.body
    );
}

#[test]
fn run_var_parses_a_value_that_looks_like_json_as_structured_data() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  read_items:
    type: transform
    jq: '$vars.items | length'
steps:
  - use: read_items
"#,
    );

    let output = test_command()
        .args([
            "run",
            workflow.path.to_str().expect("workflow path is utf-8"),
            "null",
            "--var",
            r#"items=["a","b","c"]"#,
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "3");
}

#[test]
fn a_jq_filter_can_reference_a_run_var_via_dollar_vars() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  read_lang:
    type: transform
    jq: '$vars.lang'
steps:
  - use: read_lang
"#,
    );

    let output = test_command()
        .args([
            "run",
            workflow.path.to_str().expect("workflow path is utf-8"),
            "null",
            "--var",
            "lang=英語",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "英語");
}

#[test]
fn run_rejects_a_malformed_var_with_a_clear_error() {
    let workflow = WorkflowFile::new(
        r#"
nodes:
  echo:
    type: transform
    jq: '.'
steps:
  - use: echo
"#,
    );

    let output = test_command()
        .args([
            "run",
            workflow.path.to_str().expect("workflow path is utf-8"),
            "hello",
            "--var",
            "no-equals-sign",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--var no-equals-sign"), "stderr: {stderr}");
}
