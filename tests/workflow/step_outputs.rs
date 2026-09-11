use super::*;

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
nodes:
  extract:
    type: prompt
    prompt: "{{{{ input }}}}"
    output_schema: city
    schema_name: city
  greet:
    type: prompt
    prompt: "city was {{{{ steps.extract.city }}}}"
steps:
  - id: extract
    use: extract
  - use: greet
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
nodes:
  check:
    type: transform
    jq: '{ ok: true }'
  read_check:
    type: transform
    jq: '$steps.check.ok'
steps:
  - id: check
    use: check
  - use: read_check
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
nodes:
  inner:
    type: transform
    jq: '"branch value"'
  read_inner:
    type: transform
    jq: '$steps.inner'
steps:
  - parallel:
      branches:
        - steps:
            - id: inner
              use: inner
  - use: read_inner
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
