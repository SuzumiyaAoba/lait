use super::{StepOutputs, eval_when, parse_workflow};
use crate::schema::JsonSchemaEntry;

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
    prompt: "summarize: {{ input }}"
  translate:
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
    assert_eq!(workflow.nodes["translate"].model.as_deref(), Some("cloud"));
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
    prompt: "{{ input }}"
  echo_cloud:
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

#[test]
fn parses_a_node_with_output_schema_and_jq() {
    let workflow = parse_workflow(
        r#"
nodes:
  answer:
    prompt: "{{ input }}"
    output_schema: schema.json
    schema_name: answer
    jq: ".answer"
steps:
  - use: answer
"#,
    )
    .expect("workflow with output_schema and jq should parse");

    let node = &workflow.nodes["answer"];
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
    assert_eq!(
        workflow.nodes["answer"].output_schema.as_deref(),
        Some("answer")
    );
}

#[test]
fn parses_a_workflow_with_file_path_json_schemas() {
    let workflow = parse_workflow(
        r#"
json_schemas:
  answer:
    file_path: schema.json
nodes:
  answer:
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
    jq: ".answer"
steps:
  - use: transform
"#,
    )
    .expect("a jq-only node should parse");

    assert!(workflow.nodes["transform"].prompt.is_none());
    assert_eq!(workflow.nodes["transform"].jq.as_deref(), Some(".answer"));
}

#[test]
fn rejects_a_step_with_neither_use_nor_a_router_nor_stop_or_break() {
    assert!(parse_workflow("steps:\n  - id: empty\n").is_err());
}

#[test]
fn rejects_a_node_with_neither_prompt_nor_jq() {
    let result = parse_workflow("nodes:\n  n: {}\nsteps:\n  - use: n\n");
    assert!(result.is_err());
}

#[test]
fn rejects_output_schema_without_a_prompt() {
    let result = parse_workflow(
        "nodes:\n  n:\n    jq: \".\"\n    output_schema: schema.json\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_schema_name_without_output_schema() {
    let result = parse_workflow(
        "nodes:\n  n:\n    prompt: \"{{ input }}\"\n    schema_name: answer\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_node_with_an_agent() {
    let workflow = parse_workflow(
        r#"
nodes:
  extract:
    agent: agents/extract.md
    jq: ".city"
steps:
  - use: extract
"#,
    )
    .expect("workflow with an agent node should parse");

    assert_eq!(
        workflow.nodes["extract"]
            .agent
            .as_deref()
            .and_then(|p| p.to_str()),
        Some("agents/extract.md")
    );
}

#[test]
fn rejects_a_node_with_both_prompt_and_agent() {
    let result = parse_workflow(
        "nodes:\n  n:\n    prompt: \"{{ input }}\"\n    agent: agents/extract.md\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_node_with_an_input_schema() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
    input_schema: schema.json
steps:
  - use: n
"#,
    )
    .expect("workflow with an input_schema should parse");

    assert_eq!(
        workflow.nodes["n"].input_schema.as_deref(),
        Some("schema.json")
    );
}

#[test]
fn rejects_a_node_with_agent_and_input_schema() {
    let result = parse_workflow(
        "nodes:\n  n:\n    agent: agents/extract.md\n    input_schema: schema.json\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_node_with_agent_and_output_schema() {
    let result = parse_workflow(
        "nodes:\n  n:\n    agent: agents/extract.md\n    output_schema: schema.json\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_node_with_agent_and_schema_name() {
    let result = parse_workflow(
        "nodes:\n  n:\n    agent: agents/extract.md\n    schema_name: answer\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_node_with_a_workflow() {
    let workflow = parse_workflow(
        r#"
nodes:
  sub:
    workflow: ./shared/summarize.yml
    jq: '.'
steps:
  - id: sub
    use: sub
"#,
    )
    .expect("workflow with a 'workflow' node should parse");

    assert_eq!(
        workflow.nodes["sub"]
            .workflow
            .as_ref()
            .and_then(|path| path.to_str()),
        Some("./shared/summarize.yml")
    );
}

#[test]
fn rejects_a_node_with_both_workflow_and_prompt() {
    let result = parse_workflow(
        "nodes:\n  n:\n    prompt: \"{{ input }}\"\n    workflow: sub.yml\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_node_with_both_workflow_and_agent() {
    let result = parse_workflow(
        "nodes:\n  n:\n    agent: agents/extract.md\n    workflow: sub.yml\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_node_with_model() {
    let result = parse_workflow(
        "nodes:\n  n:\n    workflow: sub.yml\n    model: local\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_node_with_input_schema() {
    let result = parse_workflow(
        "nodes:\n  n:\n    workflow: sub.yml\n    input_schema: schema.json\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_node_with_retry() {
    let result = parse_workflow(
        "nodes:\n  n:\n    workflow: sub.yml\n    retry:\n      max_attempts: 2\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_node_with_mcp() {
    let result = parse_workflow(
        "nodes:\n  n:\n    workflow: sub.yml\n    mcp: [filesystem]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_node_with_max_tool_rounds() {
    let result = parse_workflow(
        "nodes:\n  n:\n    workflow: sub.yml\n    max_tool_rounds: 4\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_mcp_on_a_jq_only_node() {
    let result =
        parse_workflow("nodes:\n  n:\n    jq: '.'\n    mcp: [filesystem]\nsteps:\n  - use: n\n");
    assert!(result.is_err());
}

#[test]
fn allows_mcp_on_a_prompt_node() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    prompt: hi\n    mcp: [filesystem]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_mcp_on_an_agent_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    agent: agents/a.md\n    mcp: [filesystem]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_a_workflow_node_with_skills() {
    let result = parse_workflow(
        "nodes:\n  n:\n    workflow: sub.yml\n    skills: [code-review]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_skills_on_a_jq_only_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    jq: '.'\n    skills: [code-review]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn allows_skills_on_a_prompt_node() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    prompt: hi\n    skills: [code-review]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_skills_on_an_agent_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    agent: agents/a.md\n    skills: [code-review]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_a_workflow_node_with_subagents() {
    let result = parse_workflow(
        "nodes:\n  n:\n    workflow: sub.yml\n    subagents: [researcher]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_subagents_on_a_jq_only_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    jq: '.'\n    subagents: [researcher]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn allows_subagents_on_a_prompt_node() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    prompt: hi\n    subagents: [researcher]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn allows_subagents_on_an_agent_node() {
    let result = parse_workflow(
        "nodes:\n  n:\n    agent: agents/a.md\n    subagents: [researcher]\nsteps:\n  - use: n\n",
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_a_node_max_tool_rounds_of_zero() {
    let result = parse_workflow(
        "default:\n  model: local\nnodes:\n  n:\n    prompt: hi\n    max_tool_rounds: 0\nsteps:\n  - use: n\n",
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_default_max_tool_rounds_of_zero() {
    let result = parse_workflow("default:\n  max_tool_rounds: 0\nsteps:\n  - use: n\n");
    assert!(result.is_err());
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

#[test]
fn rejects_a_workflow_combined_with_switch() {
    let result = parse_workflow(
        r#"
nodes:
  sub:
    workflow: sub.yml
steps:
  - use: sub
    switch:
      cases:
        - when: 'true'
          steps:
            - use: sub
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_a_workflow_step_with_when_jq_and_stop() {
    let result = parse_workflow(
        r#"
nodes:
  sub:
    workflow: sub.yml
    jq: '.'
steps:
  - when: 'true'
    use: sub
    stop: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_unknown_top_level_field() {
    let result = parse_workflow(
        r#"
unexpected: true
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_step_with_a_when_guard() {
    let workflow = parse_workflow(
        r#"
nodes:
  maybe:
    prompt: "{{ input }}"
steps:
  - id: maybe
    when: '. != null'
    use: maybe
"#,
    )
    .expect("workflow with a 'when' guard should parse");

    assert_eq!(workflow.steps[0].when.as_deref(), Some(". != null"));
}

#[test]
fn parses_a_switch_with_cases_and_else() {
    let workflow = parse_workflow(
        r#"
nodes:
  escalate:
    prompt: "escalate: {{ input }}"
  reply:
    prompt: "reply: {{ input }}"
  summarize:
    jq: ".summary"
steps:
  - id: route
    switch:
      cases:
        - id: high
          when: '.severity == "high"'
          steps:
            - use: escalate
        - when: '.severity == "medium"'
          steps:
            - use: reply
      else:
        - use: summarize
"#,
    )
    .expect("workflow with a switch should parse");

    let switch = workflow.steps[0]
        .switch
        .as_ref()
        .expect("step should have a switch");
    assert_eq!(switch.cases.len(), 2);
    assert_eq!(switch.cases[0].id.as_deref(), Some("high"));
    assert!(switch.else_steps.is_some());
}

#[test]
fn parses_a_switch_without_else() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    )
    .expect("workflow with a switch without else should parse");

    assert!(
        workflow.steps[0]
            .switch
            .as_ref()
            .unwrap()
            .else_steps
            .is_none()
    );
}

#[test]
fn rejects_a_switch_with_empty_cases() {
    let result = parse_workflow(
        r#"
steps:
  - switch:
      cases: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_case_with_empty_steps() {
    let result = parse_workflow(
        r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_with_an_empty_else() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: n
      else: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_combined_with_use() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
    switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_combined_with_when() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - when: 'true'
    switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_switch_combined_with_on_error() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - on_error:
      steps:
        - use: n
    switch:
      cases:
        - when: 'true'
          steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_a_switch_case() {
    let result = parse_workflow(
        r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_parallel_with_branches_and_join() {
    let workflow = parse_workflow(
        r#"
nodes:
  a:
    prompt: "a: {{ input }}"
  b:
    prompt: "b: {{ input }}"
steps:
  - id: fan-out
    parallel:
      branches:
        - id: a
          steps:
            - use: a
        - id: b
          steps:
            - use: b
      join: '.a + .b'
"#,
    )
    .expect("workflow with a parallel step should parse");

    let parallel = workflow.steps[0]
        .parallel
        .as_ref()
        .expect("step should have a parallel");
    assert_eq!(parallel.branches.len(), 2);
    assert_eq!(parallel.branches[0].id.as_deref(), Some("a"));
    assert_eq!(parallel.join.as_deref(), Some(".a + .b"));
}

#[test]
fn parses_a_parallel_without_join() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    jq: "."
steps:
  - parallel:
      branches:
        - steps:
            - use: n
        - steps:
            - use: n
"#,
    )
    .expect("workflow with a parallel step without join should parse");

    assert!(workflow.steps[0].parallel.as_ref().unwrap().join.is_none());
}

#[test]
fn parallel_branch_label_defaults_to_branch_n() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    jq: "."
steps:
  - parallel:
      branches:
        - steps:
            - use: n
        - id: named
          steps:
            - use: n
"#,
    )
    .expect("workflow with a parallel step should parse");

    let branches = &workflow.steps[0].parallel.as_ref().unwrap().branches;
    assert_eq!(branches[0].label(0), "branch-1");
    assert_eq!(branches[1].label(1), "named");
}

#[test]
fn rejects_a_parallel_with_empty_branches() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_parallel_branch_with_empty_steps() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_parallel_with_duplicate_branch_ids() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: "."
steps:
  - parallel:
      branches:
        - id: same
          steps:
            - use: n
        - id: same
          steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_parallel_combined_with_use() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
    parallel:
      branches:
        - steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_parallel_combined_with_stop() {
    let result = parse_workflow(
        r#"
steps:
  - stop: true
    parallel:
      branches:
        - steps:
            - stop: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_step_with_both_switch_and_parallel() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: "."
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: n
    parallel:
      branches:
        - steps:
            - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_loop_with_while_and_max_iterations() {
    let workflow = parse_workflow(
        r#"
nodes:
  bump:
    jq: '.score += 1'
steps:
  - id: refine
    loop:
      while: '.score < 3'
      max_iterations: 5
      steps:
        - use: bump
"#,
    )
    .expect("workflow with a while loop should parse");

    let loop_def = workflow.steps[0]
        .r#loop
        .as_ref()
        .expect("step should have a loop");
    assert_eq!(loop_def.r#while.as_deref(), Some(".score < 3"));
    assert!(loop_def.until.is_none());
    assert_eq!(loop_def.max_iterations, Some(5));
}

#[test]
fn parses_a_loop_with_until() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - loop:
      until: '.valid == true'
      max_iterations: 3
      steps:
        - use: n
"#,
    )
    .expect("workflow with an until loop should parse");

    let loop_def = workflow.steps[0].r#loop.as_ref().unwrap();
    assert_eq!(loop_def.until.as_deref(), Some(".valid == true"));
    assert!(loop_def.r#while.is_none());
}

#[test]
fn rejects_a_loop_with_both_while_and_until() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - loop:
      while: 'true'
      until: 'true'
      max_iterations: 3
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_with_neither_while_nor_until() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - loop:
      max_iterations: 3
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_with_no_max_iterations() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - loop:
      until: 'true'
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_with_max_iterations_zero() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - loop:
      until: 'true'
      max_iterations: 0
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_with_empty_steps() {
    let result = parse_workflow(
        r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_loop_combined_with_use() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
    loop:
      until: 'true'
      max_iterations: 3
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_a_loop() {
    let result = parse_workflow(
        r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_for_each_with_items_and_join() {
    let workflow = parse_workflow(
        r#"
nodes:
  bump:
    jq: '. + 1'
steps:
  - id: process
    for_each:
      items: '.items'
      steps:
        - use: bump
      join: 'map(. * 2)'
"#,
    )
    .expect("workflow with a for_each should parse");

    let for_each = workflow.steps[0]
        .for_each
        .as_ref()
        .expect("step should have a for_each");
    assert_eq!(for_each.items, ".items");
    assert_eq!(for_each.join.as_deref(), Some("map(. * 2)"));
}

#[test]
fn parses_a_for_each_without_join() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - for_each:
      items: '.items'
      steps:
        - use: n
"#,
    )
    .expect("workflow with a for_each without join should parse");

    assert!(workflow.steps[0].for_each.as_ref().unwrap().join.is_none());
}

#[test]
fn rejects_a_for_each_with_empty_steps() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.items'
      steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_for_each_with_max_concurrency() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - for_each:
      items: '.items'
      max_concurrency: 4
      steps:
        - use: n
"#,
    )
    .expect("workflow with a for_each max_concurrency should parse");

    assert_eq!(
        workflow.steps[0].for_each.as_ref().unwrap().max_concurrency,
        Some(4)
    );
}

#[test]
fn rejects_a_for_each_with_max_concurrency_zero() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - for_each:
      items: '.items'
      max_concurrency: 0
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_break_inside_a_for_each_with_max_concurrency_above_one() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.items'
      max_concurrency: 2
      steps:
        - break: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_stop_inside_a_for_each_with_max_concurrency_above_one() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.items'
      max_concurrency: 2
      steps:
        - stop: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_break_inside_a_sequential_for_each_nested_in_a_concurrent_one() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.outer'
      max_concurrency: 2
      steps:
        - for_each:
            items: '.inner'
            steps:
              - break: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_use_of_a_write_file_node_inside_a_for_each_with_max_concurrency_above_one() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
    write_file: out.txt
steps:
  - for_each:
      items: '.items'
      max_concurrency: 2
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_use_of_a_write_file_node_inside_a_sequential_for_each() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
    write_file: out.txt
steps:
  - for_each:
      items: '.items'
      steps:
        - use: n
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_use_of_a_write_file_node_inside_a_sequential_for_each_nested_in_a_concurrent_one() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
    write_file: out.txt
steps:
  - for_each:
      items: '.outer'
      max_concurrency: 2
      steps:
        - for_each:
            items: '.inner'
            steps:
              - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_for_each_combined_with_when() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - when: 'true'
    for_each:
      items: '.items'
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_a_for_each() {
    let result = parse_workflow(
        r#"
steps:
  - for_each:
      items: '.items'
      steps:
        - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_step_with_both_loop_and_for_each() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - use: n
    for_each:
      items: '.items'
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_step_with_stop() {
    let workflow = parse_workflow(
        r#"
steps:
  - id: done
    when: '.ready'
    stop: true
"#,
    )
    .expect("workflow with a top-level 'stop' should parse");

    assert_eq!(workflow.steps[0].stop, Some(true));
}

#[test]
fn parses_a_step_with_break_inside_a_loop() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - when: '.done'
          break: true
        - use: n
"#,
    )
    .expect("workflow with 'break' inside a loop should parse");

    let loop_def = workflow.steps[0].r#loop.as_ref().unwrap();
    assert_eq!(loop_def.steps[0].r#break, Some(true));
}

#[test]
fn parses_a_step_with_break_inside_a_for_each() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    jq: '.'
steps:
  - for_each:
      items: '.items'
      steps:
        - when: '.done'
          break: true
        - use: n
"#,
    )
    .expect("workflow with 'break' inside a for_each should parse");

    let for_each = workflow.steps[0].for_each.as_ref().unwrap();
    assert_eq!(for_each.steps[0].r#break, Some(true));
}

#[test]
fn allows_break_inside_a_loop_nested_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - loop:
                until: 'true'
                max_iterations: 3
                steps:
                  - break: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_break_at_the_top_level() {
    let result = parse_workflow(
        r#"
steps:
  - break: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_break_directly_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - break: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_stop_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - stop: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_stop_inside_a_loop_nested_inside_a_parallel_branch() {
    let result = parse_workflow(
        r#"
steps:
  - parallel:
      branches:
        - steps:
            - loop:
                until: 'true'
                max_iterations: 3
                steps:
                  - stop: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_both_stop_and_break_on_the_same_step() {
    let result = parse_workflow(
        r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - stop: true
          break: true
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_step_with_neither_an_action_nor_stop_or_break() {
    let result = parse_workflow(
        r#"
steps:
  - id: empty
    when: 'true'
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_use_combined_with_stop() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
    stop: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn allows_a_bare_when_and_stop_with_no_use() {
    let result = parse_workflow(
        r#"
steps:
  - when: '.ready'
    stop: true
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn rejects_on_error_without_use() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - stop: true
    on_error:
      steps:
        - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_step_with_retry_timeout_and_on_error() {
    let workflow = parse_workflow(
        r#"
nodes:
  call:
    prompt: "{{ input }}"
    timeout: 30
    retry:
      max_attempts: 3
      delay_seconds: 1
      backoff: 2.0
  fallback:
    jq: '{ fallback: .error }'
steps:
  - id: call
    use: call
    on_error:
      steps:
        - use: fallback
"#,
    )
    .expect("workflow with retry/timeout/on_error should parse");

    let node = &workflow.nodes["call"];
    assert_eq!(node.timeout, Some(30));
    let retry = node.retry.as_ref().unwrap();
    assert_eq!(retry.max_attempts, Some(3));
    assert_eq!(retry.delay_seconds, Some(1));
    assert_eq!(retry.backoff, Some(2.0));
    assert_eq!(workflow.steps[0].on_error.as_ref().unwrap().steps.len(), 1);
}

#[test]
fn rejects_a_retry_with_no_max_attempts() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
    retry:
      delay_seconds: 1
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_retry_with_max_attempts_zero() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
    retry:
      max_attempts: 0
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_timeout_of_zero() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
    timeout: 0
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn parses_a_node_with_temperature_top_p_and_max_tokens() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    model: local
    temperature: 0.7
    top_p: 0.9
    max_tokens: 256
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    )
    .expect("workflow should parse");

    assert_eq!(workflow.nodes["n"].temperature, Some(0.7));
    assert_eq!(workflow.nodes["n"].top_p, Some(0.9));
    assert_eq!(workflow.nodes["n"].max_tokens, Some(256));
}

#[test]
fn parses_workflow_default_temperature_top_p_and_max_tokens() {
    let workflow = parse_workflow(
        r#"
default:
  model: local
  temperature: 0.5
  top_p: 0.8
  max_tokens: 128
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    )
    .expect("workflow should parse");

    assert_eq!(workflow.default.temperature, Some(0.5));
    assert_eq!(workflow.default.top_p, Some(0.8));
    assert_eq!(workflow.default.max_tokens, Some(128));
}

#[test]
fn rejects_a_node_with_an_out_of_range_temperature() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
    temperature: 2.5
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_node_with_an_out_of_range_top_p() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
    top_p: 1.5
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_node_with_a_zero_max_tokens() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
    max_tokens: 0
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_default_with_an_out_of_range_temperature() {
    let result = parse_workflow(
        r#"
default:
  temperature: -0.1
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_node_with_temperature_combined_with_workflow() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    workflow: ./sub.yml
    temperature: 0.5
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_an_on_error_with_an_empty_steps_list() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
    on_error:
      steps: []
"#,
    );
    assert!(result.is_err());
}

#[test]
fn validates_steps_nested_inside_on_error() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
    on_error:
      steps:
        - use: undefined_node
"#,
    );
    assert!(result.is_err());
}

#[test]
fn on_error_inherits_the_failing_steps_loop_context_for_break() {
    let workflow = parse_workflow(
        r#"
nodes:
  caller:
    prompt: "{{ input }}"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - use: caller
          on_error:
            steps:
              - break: true
"#,
    );
    assert!(workflow.is_ok());
}

#[test]
fn parses_a_node_with_write_file() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    prompt: "{{ input }}"
    write_file: out.txt
steps:
  - use: n
"#,
    )
    .expect("workflow with write_file should parse");

    assert_eq!(
        workflow.nodes["n"].write_file,
        Some(std::path::PathBuf::from("out.txt"))
    );
}

#[test]
fn allows_a_node_with_only_write_file() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    write_file: out.txt
steps:
  - use: n
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn parses_a_workflow_default_retry_and_timeout() {
    let workflow = parse_workflow(
        r#"
default:
  retry:
    max_attempts: 3
    delay_seconds: 1
    backoff: 2.0
  timeout: 30
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    )
    .expect("workflow with default retry/timeout should parse");

    let retry = workflow.default.retry.as_ref().unwrap();
    assert_eq!(retry.max_attempts, Some(3));
    assert_eq!(workflow.default.timeout, Some(30));
}

#[test]
fn rejects_a_workflow_default_retry_with_no_max_attempts() {
    let result = parse_workflow(
        r#"
default:
  retry:
    delay_seconds: 1
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_default_retry_with_max_attempts_zero() {
    let result = parse_workflow(
        r#"
default:
  retry:
    max_attempts: 0
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

#[test]
fn rejects_a_workflow_default_timeout_of_zero() {
    let result = parse_workflow(
        r#"
default:
  timeout: 0
nodes:
  n:
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    );
    assert!(result.is_err());
}

// -- nodes/steps split: node resolution, reuse, and legacy-schema detection --

#[test]
fn rejects_a_use_of_an_undefined_node() {
    let result = parse_workflow(
        r#"
steps:
  - use: missing
"#,
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("missing"), "error was: {error}");
}

#[test]
fn allows_the_same_node_to_be_used_from_multiple_steps_sites() {
    let workflow = parse_workflow(
        r#"
nodes:
  greet:
    prompt: "hello: {{ input }}"
steps:
  - use: greet
  - switch:
      cases:
        - when: 'true'
          steps:
            - use: greet
      else:
        - use: greet
"#,
    )
    .expect("reusing a node from multiple steps sites should parse");

    assert_eq!(workflow.nodes.len(), 1);
}

#[test]
fn rejects_a_site_id_colliding_with_a_different_node_id() {
    let result = parse_workflow(
        r#"
nodes:
  draft:
    prompt: "draft: {{ input }}"
  summarize:
    jq: '.'
steps:
  - id: summarize
    use: draft
"#,
    );
    assert!(result.is_err());
}

#[test]
fn allows_a_site_id_equal_to_its_own_used_node_id() {
    let result = parse_workflow(
        r#"
nodes:
  draft:
    prompt: "draft: {{ input }}"
steps:
  - id: draft
    use: draft
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn eval_when_coerces_plain_text_input_to_a_json_string() {
    assert!(eval_when(". == \"hello\"", "hello", &StepOutputs::new()).unwrap());
    assert!(!eval_when(". == \"hello\"", "world", &StepOutputs::new()).unwrap());
}

#[test]
fn eval_when_evaluates_against_parsed_json_input() {
    assert!(eval_when(".flag", r#"{"flag":true}"#, &StepOutputs::new()).unwrap());
    assert!(!eval_when(".flag", r#"{"flag":false}"#, &StepOutputs::new()).unwrap());
}

#[test]
fn eval_when_can_reference_a_named_step_output_via_dollar_steps() {
    let mut steps = StepOutputs::new();
    steps.insert("check".to_owned(), serde_json::json!({"ok": true}));
    assert!(eval_when("$steps.check.ok", "null", &steps).unwrap());
}
