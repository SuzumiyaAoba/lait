use super::*;

#[test]
fn omits_version_and_still_parses_as_the_latest_schema() {
    let result =
        parse_workflow("nodes:\n  n:\n    type: transform\n    jq: '.'\nsteps:\n  - use: n\n");
    assert!(result.is_ok());
}

#[test]
fn parses_a_workflow_with_an_explicit_matching_version() {
    let result = parse_workflow(
        "version: 1\nnodes:\n  n:\n    type: transform\n    jq: '.'\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_an_unrecognized_workflow_version() {
    let result = parse_workflow(
        "version: 99\nnodes:\n  n:\n    type: transform\n    jq: '.'\nsteps:\n  - use: n\n",
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("version"), "error was: {error}");
}

#[test]
fn rejects_a_node_with_no_type() {
    let result = parse_workflow("nodes:\n  n: {}\nsteps:\n  - use: n\n");
    let error = result.unwrap_err().to_string();
    assert!(error.contains("type"), "error was: {error}");
    assert!(
        error.contains("prompt/agent/workflow/command/transform"),
        "error was: {error}"
    );
}

#[test]
fn rejects_an_unrecognized_node_type() {
    let result = parse_workflow("nodes:\n  n:\n    type: bogus\nsteps:\n  - use: n\n");
    assert!(result.is_err());
}

#[test]
fn rejects_an_unknown_field_on_a_prompt_node() {
    // `agent:` is not a `PromptNode` field — `#[serde(deny_unknown_fields)]`
    // catches it at parse time now, in place of the old flat struct's
    // runtime "can have at most one of ..." check.
    let result = parse_workflow(
        "nodes:\n  n:\n    type: prompt\n    prompt: hi\n    agent: agents/a.md\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_an_unknown_field_on_an_agent_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    type: agent\n    agent: agents/a.md\n    system_prompt: hi\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_an_unknown_field_on_a_workflow_node() {
    // `WorkflowNode` has no `model`/sampling/capability/`retry`/`timeout`/
    // schema/attachment fields at all — every model-call knob belongs on the
    // referenced sub-workflow's own steps instead (see `WorkflowNode`'s doc
    // comment).
    let result = parse_workflow(
        "nodes:\n  n:\n    type: workflow\n    workflow: sub.yml\n    model: local\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_node_with_retry() {
    // `retry`/`timeout` specifically (not just an arbitrary unknown field):
    // apply to a single action and must be set on the steps inside the
    // referenced workflow file instead — see `allows_a_workflow_node_with_on_error_at_its_use_site`.
    let result = parse_workflow(
        "nodes:\n  n:\n    type: workflow\n    workflow: sub.yml\n    retry:\n      max_attempts: 2\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_an_unknown_field_on_a_command_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    type: command\n    command: [\"wc\"]\n    files: [notes.txt]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_an_unknown_field_on_a_transform_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    type: transform\n    jq: '.'\n    mcp: [filesystem]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn allows_mcp_on_a_prompt_node() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    type: prompt\n    prompt: hi\n    mcp: [filesystem]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_mcp_on_an_agent_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    type: agent\n    agent: agents/a.md\n    mcp: [filesystem]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_skills_on_a_prompt_node() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    type: prompt\n    prompt: hi\n    skills: [code-review]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_skills_on_an_agent_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    type: agent\n    agent: agents/a.md\n    skills: [code-review]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_subagents_on_a_prompt_node() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    type: prompt\n    prompt: hi\n    subagents: [researcher]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_subagents_on_an_agent_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    type: agent\n    agent: agents/a.md\n    subagents: [researcher]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_a_system_prompt_only_node_with_no_prompt() {
    let workflow = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    type: prompt\n    system_prompt: be terse\nsteps:\n  - use: n\n",
    )
    .expect("workflow should parse");
    let NodeDefinition::Prompt(n) = workflow.nodes["n"].as_ref() else {
        panic!("expected a prompt node");
    };
    assert!(n.prompt.is_none());
    assert_eq!(n.system_prompt.as_deref(), Some("be terse"));
}

#[test]
fn allows_system_prompt_together_with_jq_as_a_model_calling_node() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    type: prompt\n    jq: '.'\n    system_prompt: be terse\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_system_prompt_on_a_prompt_node() {
    let workflow = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    type: prompt\n    prompt: hi\n    system_prompt: be terse\nsteps:\n  - use: n\n",
    )
    .expect("workflow should parse");
    let NodeDefinition::Prompt(n) = workflow.nodes["n"].as_ref() else {
        panic!("expected a prompt node");
    };
    assert_eq!(n.system_prompt.as_deref(), Some("be terse"));
}

#[test]
fn rejects_a_prompt_node_with_neither_prompt_nor_system_prompt() {
    let result = parse_workflow("nodes:\n  n:\n    type: prompt\nsteps:\n  - use: n\n");
    let error = result.unwrap_err().to_string();
    assert!(error.contains("transform"), "error was: {error}");
}

#[test]
fn rejects_a_transform_node_with_neither_jq_nor_write_file() {
    let result = parse_workflow("nodes:\n  n:\n    type: transform\nsteps:\n  - use: n\n");
    assert!(result.is_err());
}

#[test]
fn parses_a_node_with_output_schema_and_jq() {
    let workflow = parse_workflow(
        r#"
nodes:
  answer:
    type: prompt
    prompt: "{{ input }}"
    output_schema: schema.json
    schema_name: answer
    jq: ".answer"
steps:
  - use: answer
"#,
    )
    .expect("workflow with output_schema and jq should parse");

    let NodeDefinition::Prompt(node) = workflow.nodes["answer"].as_ref() else {
        panic!("expected a prompt node");
    };
    assert_eq!(node.output_schema.as_deref(), Some("schema.json"));
    assert_eq!(node.schema_name.as_deref(), Some("answer"));
    assert_eq!(node.jq.as_deref(), Some(".answer"));
}

#[test]
fn parses_a_workflow_with_inline_json_schemas() {
    let workflow = parse_workflow(
        r#"
json_schemas:
  answer:
    schema:
      type: object
      properties:
        answer:
          type: string
      required: [answer]
nodes:
  answer:
    type: prompt
    prompt: "{{ input }}"
    output_schema: answer
steps:
  - use: answer
"#,
    )
    .expect("workflow with inline json_schemas should parse");

    assert_eq!(workflow.json_schemas.len(), 1);
    match &workflow.json_schemas["answer"] {
        JsonSchemaEntry::Inline { schema } => {
            assert_eq!(schema["properties"]["answer"]["type"], "string");
        }
        JsonSchemaEntry::FilePath { .. } => panic!("expected an inline schema entry"),
    }
    let NodeDefinition::Prompt(answer) = workflow.nodes["answer"].as_ref() else {
        panic!("expected a prompt node");
    };
    assert_eq!(answer.output_schema.as_deref(), Some("answer"));
}
