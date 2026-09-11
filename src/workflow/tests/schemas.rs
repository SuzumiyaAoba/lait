use super::*;

#[test]
fn parses_a_workflow_with_file_path_json_schemas() {
    let workflow = parse_workflow(
        r#"
json_schemas:
  answer:
    file_path: schema.json
nodes:
  answer:
    type: prompt
    prompt: "{{ input }}"
    output_schema: answer
steps:
  - use: answer
"#,
    )
    .expect("workflow with file_path json_schemas should parse");

    match &workflow.json_schemas["answer"] {
        JsonSchemaEntry::FilePath { file_path } => {
            assert_eq!(file_path.to_str(), Some("schema.json"));
        }
        JsonSchemaEntry::Inline { .. } => panic!("expected a file_path schema entry"),
    }
}

#[test]
fn rejects_a_json_schemas_entry_with_both_schema_and_file_path() {
    let result = parse_workflow(
        r#"
json_schemas:
  answer:
    schema:
      type: object
    file_path: schema.json
nodes:
  answer:
    type: prompt
    prompt: "{{ input }}"
    output_schema: answer
steps:
  - use: answer
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_json_schemas_entry_with_neither_schema_nor_file_path() {
    let result = parse_workflow(
        r#"
json_schemas:
  answer: {}
nodes:
  answer:
    type: prompt
    prompt: "{{ input }}"
    output_schema: answer
steps:
  - use: answer
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_a_transform_only_node_with_no_prompt() {
    let workflow = parse_workflow(
        r#"
nodes:
  transform:
    type: transform
    jq: ".answer"
steps:
  - use: transform
"#,
    )
    .expect("a jq-only node should parse");

    assert_eq!(workflow.nodes["transform"].settings().jq, Some(".answer"));
}

#[test]
fn rejects_a_step_with_neither_use_nor_a_router_nor_stop_or_break() {
    assert!(parse_workflow("steps:\n  - id: empty\n").is_err());
}

#[test]
fn rejects_output_schema_on_a_transform_node() {
    // `output_schema` only exists on `PromptNode` now — setting it on a
    // `type: transform` node is an unknown-field parse error, in place of
    // the old flat struct's "no prompt/system_prompt/agent to apply it to"
    // runtime bail.
    let result = parse_workflow(
        "nodes:\n  n:\n    type: transform\n    jq: \".\"\n    output_schema: schema.json\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_schema_name_without_output_schema() {
    let result = parse_workflow(
        "nodes:\n  n:\n    type: prompt\n    prompt: \"{{ input }}\"\n    schema_name: answer\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_node_with_an_agent() {
    let workflow = parse_workflow(
        r#"
nodes:
  extract:
    type: agent
    agent: agents/extract.md
    jq: ".city"
steps:
  - use: extract
"#,
    )
    .expect("workflow with an agent node should parse");

    let NodeDefinition::Agent(extract) = workflow.nodes["extract"].as_ref() else {
        panic!("expected an agent node");
    };
    assert_eq!(extract.agent.to_str(), Some("agents/extract.md"));
}

#[test]
fn parses_a_node_with_an_input_schema() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    input_schema: schema.json
steps:
  - use: n
"#,
    )
    .expect("workflow with an input_schema should parse");

    let NodeDefinition::Prompt(n) = workflow.nodes["n"].as_ref() else {
        panic!("expected a prompt node");
    };
    assert_eq!(n.input_schema.as_deref(), Some("schema.json"));
}

#[test]
fn parses_a_node_with_a_workflow() {
    let workflow = parse_workflow(
        r#"
nodes:
  sub:
    type: workflow
    workflow: ./shared/summarize.yml
    jq: '.'
steps:
  - id: sub
    use: sub
"#,
    )
    .expect("workflow with a 'workflow' node should parse");

    let NodeDefinition::Workflow(sub) = workflow.nodes["sub"].as_ref() else {
        panic!("expected a workflow node");
    };
    assert_eq!(sub.workflow.to_str(), Some("./shared/summarize.yml"));
}

#[test]
fn allows_a_workflow_node_with_on_error_at_its_use_site() {
    // `on_error` lives on the `steps[]` reference site, not on the node
    // (unlike `retry`/`timeout`, which stay forbidden on a `workflow:` node —
    // see `rejects_a_workflow_node_with_retry` above), so it's free to catch
    // a `workflow:` node's sub-workflow failing as a whole.
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: workflow
    workflow: sub.yml
steps:
  - use: n
    on_error:
      steps:
        - use: n
"#,
    );
    assert!(result.is_ok());
}
