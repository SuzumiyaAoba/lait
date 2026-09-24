use super::*;

#[test]
fn run_input_fills_a_prompt_templates_inputs_placeholder() {
    let server = MockServer::start("200 OK", CHAT_COMPLETION_BODY);
    let workflow = WorkflowFile::new(&format!(
        r#"
inputs:
  lang: string
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
steps:
  - id: greet
    prompt: "{{{{ input }}}} in {{{{ inputs.lang }}}}"
"#,
        server.base_url
    ));

    let output = run_workflow_with(&workflow, &["hello", "--input", "lang=英語"]);
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
fn a_later_input_wins_when_the_same_key_is_passed_twice() {
    let workflow = WorkflowFile::new("inputs:\n  lang: string\nsteps:\n  - jq: '$inputs.lang'\n");

    let output = run_workflow_with(&workflow, &["--input", "lang=ja", "--input", "lang=en"]);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "en");
}

#[test]
fn an_input_is_parsed_according_to_its_declared_type() {
    let workflow = WorkflowFile::new(
        r#"
inputs:
  id: string
  items: { type: array, items: { type: string } }
  limit: { type: integer, default: 10 }
steps:
  - jq: '{id: $inputs.id, count: ($inputs.items | length), limit: $inputs.limit}'
"#,
    );

    let output = run_workflow_with(
        &workflow,
        &["--input", "id=0012", "--input", r#"items=["a","b","c"]"#],
    );

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"id":"0012","count":3,"limit":10}"#,
        "a string input stays verbatim, others parse as JSON, defaults apply"
    );
}

#[test]
fn a_workflow_with_inputs_runs_without_a_prompt() {
    let workflow =
        WorkflowFile::new("inputs:\n  name: string\nsteps:\n  - jq: '[., $inputs.name]'\n");

    let output = run_workflow_with(&workflow, &["--input", "name=lait"]);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"[null,"lait"]"#
    );
}

#[test]
fn run_rejects_missing_undeclared_and_mistyped_inputs() {
    let workflow = WorkflowFile::new(
        "inputs:\n  count: integer\n  name: { type: string, description: who to greet }\nsteps:\n  - jq: '.'\n",
    );

    for (args, expected) in [
        (
            vec!["--input", "count=1"],
            "missing required input 'name' (who to greet)",
        ),
        (
            vec![
                "--input", "count=1", "--input", "name=x", "--input", "nmae=y",
            ],
            "unknown input 'nmae'",
        ),
        (
            vec!["--input", "count=many", "--input", "name=x"],
            "inputs.count must be of type integer",
        ),
        (
            vec!["--input", "no-equals-sign"],
            "--input \"no-equals-sign\"",
        ),
    ] {
        let output = run_workflow_with(&workflow, &args);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: stderr: {stderr}");
    }
}

#[test]
fn run_rejects_an_input_when_the_workflow_declares_none() {
    let workflow = WorkflowFile::new("steps:\n  - jq: '.'\n");

    let output = run_workflow_with(&workflow, &["hello", "--input", "lang=ja"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("declares no 'inputs:'"), "stderr: {stderr}");
}

#[test]
fn input_schema_parses_and_validates_the_prompt() {
    let workflow = WorkflowFile::new(
        "input_schema: { type: object, required: [n] }\nsteps:\n  - jq: '.n + 1'\n",
    );

    let output = run_workflow_with(&workflow, &[r#"{"n": 41}"#]);
    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");

    let output = run_workflow_with(&workflow, &["not json"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must be JSON"),
        "{output:?}"
    );

    let output = run_workflow_with(&workflow, &[r#"{"m": 1}"#]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing required field(s): n"),
        "{output:?}"
    );
}

#[test]
fn without_an_input_schema_a_json_looking_prompt_stays_a_string() {
    let workflow = WorkflowFile::new("steps:\n  - jq: 'type'\n");

    let output = run_workflow_with(&workflow, &["42"]);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "string");
}

#[test]
fn a_child_workflow_receives_with_inputs_and_validates_its_input_schema() {
    let child = WorkflowFile::new(
        r#"
input_schema: { type: array }
inputs:
  sep: string
output: 'join($inputs.sep)'
steps:
  - jq: 'map(ascii_upcase)'
"#,
    );
    let child_name = child.path.file_name().unwrap().to_str().unwrap();
    let parent = WorkflowFile::new(&format!(
        r#"
steps:
  - jq: '["a", "b"]'
  - workflow: ./{child_name}
    with: '{{sep: "-"}}'
  - workflow: ./{child_name}
    input: '"not an array"'
    with: '{{sep: "-"}}'
    on_error:
      - jq: '[.input, (.error | contains("must be of type array"))]'
"#
    ));

    let output = run_workflow_with(&parent, &["x"]);

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"["A-B",true]"#
    );
}
