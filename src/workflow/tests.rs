//! Parser/validator tests for the version 2 workflow format. Execution is
//! covered by `tests/workflow.rs`.

use super::model::*;
use super::parse_workflow;

fn parse(yaml: &str) -> WorkflowFile {
    parse_workflow(yaml).unwrap_or_else(|error| panic!("workflow should parse: {error:#}"))
}

fn error(yaml: &str) -> String {
    match parse_workflow(yaml) {
        Ok(_) => panic!("workflow should be rejected:\n{yaml}"),
        Err(error) => format!("{error:#}"),
    }
}

fn assert_rejected(yaml: &str, expected: &str) {
    let message = error(yaml);
    assert!(
        message.contains(expected),
        "expected error containing {expected:?}, got: {message}"
    );
}

// --- document level -------------------------------------------------------

#[test]
fn parses_a_minimal_workflow() {
    let wf = parse("steps:\n  - prompt: 'hi {{ input }}'\n");
    assert_eq!(wf.steps.len(), 1);
    assert!(matches!(wf.steps[0].kind, StepKind::Prompt(_)));
    assert!(wf.inputs.is_empty());
    assert!(!wf.declares_inputs());
}

#[test]
fn parses_the_document_level_fields() {
    let wf = parse(
        r#"
version: 2
name: demo
description: a demo
inputs:
  text: string
  lang: { type: string, default: ja, description: target language }
  count: { type: integer }
output: '{text: ., lang: $inputs.lang}'
timeout: 60
default:
  model: local
  system: be brief
  temperature: 0.5
  retry: { max_attempts: 2 }
  timeout: 10
models:
  local:
    - provider: { base_url: "http://localhost:1234/v1" }
      model_id: m
schemas:
  city: { type: object, required: [city] }
  file_based: { file: city.schema.json }
agents:
  writer:
    model: local
    system: 'You write about {{ input }}'
steps:
  - agent: writer
    id: w
"#,
    );
    assert_eq!(wf.name.as_deref(), Some("demo"));
    assert_eq!(wf.timeout, Some(60));
    assert_eq!(wf.output.as_deref(), Some("{text: ., lang: $inputs.lang}"));
    let names: Vec<&str> = wf.inputs.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        ["text", "lang", "count"],
        "declaration order is kept"
    );
    assert_eq!(wf.inputs[0].1.schema, serde_json::json!({"type": "string"}));
    assert_eq!(wf.inputs[1].1.default, Some(serde_json::json!("ja")));
    assert_eq!(
        wf.inputs[1].1.description.as_deref(),
        Some("target language")
    );
    assert!(wf.inputs[2].1.default.is_none());
    assert_eq!(wf.defaults.system.as_deref(), Some("be brief"));
    assert_eq!(wf.defaults.retry.as_ref().unwrap().max_attempts, 2);
    assert!(wf.models.contains_key("local"));
    assert_eq!(
        wf.schemas["file_based"],
        crate::schema::SchemaSource::File("./city.schema.json".into())
    );
    assert_eq!(
        wf.agents["writer"].system_prompt_template,
        "You write about {{ input }}"
    );
}

#[test]
fn rejects_a_workflow_with_no_steps() {
    assert_rejected("steps: []\n", "at least one step");
    assert!(parse_workflow("name: x\n").is_err());
}

#[test]
fn rejects_unknown_top_level_fields() {
    assert_rejected("json_schemas: {}\nsteps:\n  - jq: .\n", "json_schemas");
}

#[test]
fn rejects_version_1_documents_with_a_migration_hint() {
    assert_rejected(
        "nodes:\n  a:\n    type: prompt\n    prompt: x\nsteps:\n  - use: a\n",
        "migration guide",
    );
    assert_rejected("version: 1\nsteps:\n  - jq: .\n", "version 1");
}

#[test]
fn rejects_an_unknown_version() {
    assert_rejected(
        "version: 3\nsteps:\n  - jq: .\n",
        "unsupported workflow 'version: 3'",
    );
}

#[test]
fn points_a_version_1_use_step_at_the_migration_guide() {
    assert_rejected("steps:\n  - use: a\n", "migration guide");
}

// --- step shape -----------------------------------------------------------

#[test]
fn every_step_needs_exactly_one_kind() {
    assert_rejected("steps:\n  - id: x\n", "exactly one of");
    assert_rejected(
        "steps:\n  - prompt: a\n    jq: .\n",
        "exactly one kind, found prompt and jq",
    );
    assert_rejected("steps:\n  - 'jq: .'\n", "must be a mapping");
}

#[test]
fn rejects_fields_that_do_not_belong_to_the_kind() {
    assert_rejected(
        "steps:\n  - jq: .\n    model: local\n",
        "unknown field 'model'; 'jq' steps accept",
    );
    assert_rejected(
        "steps:\n  - run: [echo]\n    system: x\n",
        "unknown field 'system'; 'run' steps accept",
    );
    assert_rejected(
        "steps:\n  - agent: a.md\n    system: x\n",
        "unknown field 'system'; 'agent' steps accept",
    );
    assert_rejected(
        "steps:\n  - agent: a.md\n    output_schema: {type: object}\n",
        "unknown field 'output_schema'",
    );
    assert_rejected(
        "steps:\n  - workflow: ./c.yml\n    model: x\n",
        "unknown field 'model'",
    );
}

#[test]
fn stop_and_break_take_no_retry_timeout_or_on_error() {
    assert_rejected(
        "steps:\n  - stop: true\n    retry: {max_attempts: 2}\n",
        "unknown field 'retry'; 'stop' steps accept",
    );
    assert_rejected(
        "steps:\n  - stop: true\n    on_error:\n      - jq: .\n",
        "unknown field 'on_error'",
    );
    let wf = parse("steps:\n  - stop: true\n    when: '. == 1'\n    output: '.x'\n    id: s\n");
    assert!(matches!(wf.steps[0].kind, StepKind::Stop));
    assert_eq!(wf.steps[0].output.as_deref(), Some(".x"));
}

#[test]
fn stop_and_break_only_accept_true() {
    assert_rejected("steps:\n  - stop: false\n", "only accepts 'true'");
    assert_rejected(
        "steps:\n  - until: 'true'\n    max_iterations: 1\n    steps:\n      - break: false\n",
        "only accepts 'true'",
    );
}

#[test]
fn common_fields_are_accepted_on_every_non_control_kind() {
    let wf = parse(
        r#"
steps:
  - id: g
    when: '. != null'
    input: '.a'
    output: '.b'
    retry: { max_attempts: 3, delay_seconds: 1, backoff: 2.0 }
    timeout: 5
    on_error:
      - jq: '.error'
    group:
      - jq: .
"#,
    );
    let step = &wf.steps[0];
    assert_eq!(step.id.as_deref(), Some("g"));
    assert_eq!(step.when.as_deref(), Some(". != null"));
    assert_eq!(step.input.as_deref(), Some(".a"));
    assert_eq!(step.output.as_deref(), Some(".b"));
    let retry = step.retry.as_ref().unwrap();
    assert_eq!(
        (retry.max_attempts, retry.delay_seconds, retry.backoff),
        (3, 1, 2.0)
    );
    assert_eq!(step.timeout, Some(5));
    assert_eq!(step.on_error.as_ref().unwrap().len(), 1);
}

#[test]
fn rejects_invalid_ids_and_duplicates() {
    assert_rejected("steps:\n  - id: '1st'\n    jq: .\n", "invalid step id");
    assert_rejected("steps:\n  - id: 'a b'\n    jq: .\n", "invalid step id");
    assert_rejected(
        "steps:\n  - id: a\n    jq: .\n  - group:\n      - id: a\n        jq: .\n",
        "step id 'a' is used more than once",
    );
    assert_rejected(
        "steps:\n  - id: a\n    jq: .\n    on_error:\n      - id: a\n        jq: .\n",
        "used more than once",
    );
    parse("steps:\n  - id: my-step_2\n    jq: .\n");
}

#[test]
fn rejects_invalid_jq_and_template_syntax_at_load_time() {
    assert_rejected("steps:\n  - jq: '.['\n", "'jq'");
    assert_rejected("steps:\n  - jq: .\n    when: '.['\n", "'when'");
    assert_rejected("steps:\n  - prompt: '{{ input'\n", "'prompt'");
    assert_rejected("steps:\n  - run: ['{{ x']\n", "'run'");
    assert_rejected("output: '(('\nsteps:\n  - jq: .\n", "'output'");
    assert_rejected("steps:\n  - jq: '$vars.x'\n", "'jq'");
}

#[test]
fn rejects_numeric_fields_out_of_range() {
    assert_rejected(
        "steps:\n  - jq: .\n    retry: {max_attempts: 0}\n",
        "max_attempts",
    );
    assert_rejected(
        "steps:\n  - jq: .\n    retry: {delay_seconds: 1}\n",
        "max_attempts",
    );
    assert_rejected(
        "steps:\n  - jq: .\n    retry: {max_attempts: 2, backoff: -1}\n",
        "backoff",
    );
    assert_rejected("steps:\n  - jq: .\n    timeout: 0\n", "at least 1 second");
    assert_rejected("timeout: 0\nsteps:\n  - jq: .\n", "'timeout'");
    assert_rejected(
        "default:\n  timeout: 0\nsteps:\n  - jq: .\n",
        "default.timeout",
    );
    assert_rejected(
        "default:\n  retry: {max_attempts: 0}\nsteps:\n  - jq: .\n",
        "default.retry",
    );
    assert_rejected(
        "default:\n  temperature: 3\nsteps:\n  - jq: .\n",
        "temperature",
    );
    assert_rejected("steps:\n  - prompt: x\n    top_p: 1.5\n", "top_p");
    assert_rejected("steps:\n  - prompt: x\n    max_tokens: 0\n", "max_tokens");
    assert_rejected(
        "steps:\n  - prompt: x\n    max_tool_rounds: 0\n",
        "max_tool_rounds",
    );
}

// --- action kinds ---------------------------------------------------------

#[test]
fn parses_a_prompt_step_with_every_field() {
    let wf = parse(
        r#"
schemas:
  city: { type: object }
steps:
  - prompt: 'about {{ input }}'
    system: 'be concise about {{ input }}'
    model: local
    reasoning_effort: high
    temperature: 0.2
    top_p: 0.9
    max_tokens: 100
    mcp: [fs]
    max_tool_rounds: 3
    skills: [review]
    subagents: [researcher]
    tools: [rg]
    files: [notes.md]
    images: ['https://example.com/a.png']
    input_schema: { type: string }
    output_schema: city
"#,
    );
    let StepKind::Prompt(prompt) = &wf.steps[0].kind else {
        panic!("expected a prompt step");
    };
    assert_eq!(prompt.llm.model.as_deref(), Some("local"));
    assert_eq!(prompt.llm.max_tokens, Some(100));
    assert_eq!(prompt.llm.tools.as_deref(), Some(&["rg".to_owned()][..]));
    assert_eq!(prompt.attachments.files, ["notes.md"]);
    assert!(prompt.input_schema.as_ref().unwrap().name.is_none());
    assert_eq!(
        prompt.output_schema.as_ref().unwrap().name.as_deref(),
        Some("city")
    );
    assert_eq!(
        prompt.effective_schema_name(),
        "city",
        "a named schema supplies the default schema name"
    );
}

#[test]
fn schema_references_must_name_a_defined_schema() {
    assert_rejected(
        "steps:\n  - prompt: x\n    output_schema: missing\n",
        "schema 'missing', which is not defined",
    );
    let wf = parse("steps:\n  - prompt: x\n    output_schema: {file: out.json}\n");
    let StepKind::Prompt(prompt) = &wf.steps[0].kind else {
        panic!()
    };
    assert_eq!(
        prompt.output_schema.as_ref().unwrap().source,
        crate::schema::SchemaSource::File("./out.json".into())
    );
    assert_eq!(prompt.effective_schema_name(), "structured_output");
}

#[test]
fn schema_name_requires_an_output_schema_and_a_valid_name() {
    assert_rejected(
        "steps:\n  - prompt: x\n    schema_name: y\n",
        "'schema_name' requires 'output_schema'",
    );
    assert_rejected(
        "steps:\n  - prompt: x\n    output_schema: {type: object}\n    schema_name: 'bad name'\n",
        "schema name",
    );
    assert_rejected(
        "schemas:\n  'bad.name': {type: object}\nsteps:\n  - jq: .\n",
        "invalid schema",
    );
}

#[test]
fn resolves_agent_references() {
    let wf = parse(
        r#"
agents:
  inline:
    system: hi
steps:
  - agent: inline
  - agent: ./agents/file.md
  - agent: other.md
  - agent: registered
"#,
    );
    let refs: Vec<&AgentRef> = wf
        .steps
        .iter()
        .map(|step| match &step.kind {
            StepKind::Agent(agent) => &agent.agent,
            _ => panic!("expected agent steps"),
        })
        .collect();
    assert!(matches!(refs[0], AgentRef::Inline { name, .. } if name == "inline"));
    assert!(
        matches!(refs[1], AgentRef::Path(path) if path == std::path::Path::new("./agents/file.md"))
    );
    assert!(matches!(refs[2], AgentRef::Path(path) if path == std::path::Path::new("./other.md")));
    assert!(matches!(refs[3], AgentRef::Registry(name) if name == "registered"));
}

#[test]
fn inline_agents_require_system_and_reject_unknown_fields() {
    assert_rejected(
        "agents:\n  a:\n    model: m\nsteps:\n  - agent: a\n",
        "requires a 'system:'",
    );
    assert_rejected(
        "agents:\n  a:\n    system: x\n    prompt: y\nsteps:\n  - agent: a\n",
        "prompt",
    );
    assert_rejected(
        "agents:\n  a:\n    system: '{{ x'\nsteps:\n  - agent: a\n",
        "agents.a.system",
    );
}

#[test]
fn resolves_workflow_references() {
    let wf = parse("steps:\n  - workflow: ./child.yml\n    with: '{a: 1}'\n  - workflow: shared\n");
    let StepKind::Workflow(first) = &wf.steps[0].kind else {
        panic!()
    };
    assert!(
        matches!(&first.workflow, WorkflowRef::Path(path) if path == std::path::Path::new("./child.yml"))
    );
    assert_eq!(first.with.as_deref(), Some("{a: 1}"));
    let StepKind::Workflow(second) = &wf.steps[1].kind else {
        panic!()
    };
    assert!(matches!(&second.workflow, WorkflowRef::Registry(name) if name == "shared"));
}

#[test]
fn parses_run_jq_ask_and_write_steps() {
    let wf = parse(
        r#"
steps:
  - run: [wc, -l]
  - jq: 'tonumber'
  - ask: 'ok? {{ input }}'
    choices: ['yes', 'no']
    default: 'no'
    multiline: true
  - write: 'out/{{ input }}.txt'
"#,
    );
    assert!(matches!(&wf.steps[0].kind, StepKind::Run(run) if run.argv == ["wc", "-l"]));
    assert!(matches!(&wf.steps[1].kind, StepKind::Jq(filter) if filter == "tonumber"));
    let StepKind::Ask(ask) = &wf.steps[2].kind else {
        panic!()
    };
    assert_eq!(ask.default.as_deref(), Some("no"));
    assert!(ask.multiline);
    let StepKind::Write(write) = &wf.steps[3].kind else {
        panic!()
    };
    assert!(write.is_dynamic());
}

#[test]
fn rejects_invalid_action_values() {
    assert_rejected("steps:\n  - run: []\n", "start with the program");
    assert_rejected("steps:\n  - run: [' ']\n", "start with the program");
    assert_rejected("steps:\n  - write: ''\n", "needs a path");
    assert_rejected(
        "steps:\n  - ask: q\n    choices: [a]\n    default: b\n",
        "must be one of 'choices'",
    );
    assert_rejected("steps:\n  - ask: q\n    choices: []\n", "non-empty list");
}

// --- control kinds --------------------------------------------------------

#[test]
fn parses_switch_cases_and_else() {
    let wf = parse(
        r#"
steps:
  - switch:
      - when: '.a'
        steps:
          - jq: '1'
      - when: '.b'
        steps:
          - jq: '2'
    else:
      - jq: '3'
"#,
    );
    let StepKind::Switch(switch) = &wf.steps[0].kind else {
        panic!()
    };
    assert_eq!(switch.cases.len(), 2);
    assert_eq!(switch.cases[1].when, ".b");
    assert_eq!(switch.else_steps.as_ref().unwrap().len(), 1);
}

#[test]
fn rejects_malformed_switches() {
    assert_rejected("steps:\n  - switch: []\n", "at least one case");
    assert_rejected(
        "steps:\n  - switch:\n      - when: .a\n",
        "a case requires 'steps'",
    );
    assert_rejected(
        "steps:\n  - switch:\n      - when: .a\n        steps: []\n",
        "at least one step",
    );
    assert_rejected(
        "steps:\n  - switch:\n      - when: .a\n        steps: [{jq: .}]\n        id: x\n",
        "unknown field 'id'; switch cases accept",
    );
    assert_rejected(
        "steps:\n  - switch:\n      - when: .a\n        steps: [{jq: .}]\n    else: []\n",
        "at least one step",
    );
}

#[test]
fn parses_parallel_branches_in_declaration_order() {
    let wf = parse(
        "steps:\n  - parallel:\n      zeta:\n        - jq: '1'\n      alpha:\n        - jq: '2'\n",
    );
    let StepKind::Parallel(parallel) = &wf.steps[0].kind else {
        panic!()
    };
    let names: Vec<&str> = parallel
        .branches
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(names, ["zeta", "alpha"]);
}

#[test]
fn rejects_malformed_parallels() {
    assert_rejected("steps:\n  - parallel: {}\n", "at least one branch");
    assert_rejected("steps:\n  - parallel: [a]\n", "map branch names");
    assert_rejected(
        "steps:\n  - parallel:\n      'a b':\n        - jq: .\n",
        "invalid branch name",
    );
    assert_rejected("steps:\n  - parallel:\n      a: []\n", "at least one step");
}

#[test]
fn parses_loops_and_for_each() {
    let wf = parse(
        r#"
steps:
  - for_each: '.items'
    max_concurrency: 4
    steps:
      - jq: .
  - while: '. < 3'
    max_iterations: 5
    steps:
      - jq: '. + 1'
  - until: '. >= 3'
    max_iterations: 5
    steps:
      - jq: '. + 1'
"#,
    );
    let StepKind::ForEach(for_each) = &wf.steps[0].kind else {
        panic!()
    };
    assert_eq!(
        (for_each.items.as_str(), for_each.max_concurrency),
        (".items", 4)
    );
    let StepKind::Loop(while_loop) = &wf.steps[1].kind else {
        panic!()
    };
    assert!(matches!(&while_loop.condition, LoopCondition::While(c) if c == ". < 3"));
    assert_eq!(wf.steps[1].kind.name(), "while");
    let StepKind::Loop(until_loop) = &wf.steps[2].kind else {
        panic!()
    };
    assert_eq!(until_loop.condition.keyword(), "until");
    assert_eq!(until_loop.max_iterations, 5);
}

#[test]
fn a_for_each_defaults_to_sequential() {
    let wf = parse("steps:\n  - for_each: .\n    steps:\n      - jq: .\n");
    let StepKind::ForEach(for_each) = &wf.steps[0].kind else {
        panic!()
    };
    assert_eq!(for_each.max_concurrency, 1);
}

#[test]
fn rejects_malformed_loops() {
    assert_rejected("steps:\n  - for_each: .\n", "requires 'steps'");
    assert_rejected(
        "steps:\n  - for_each: .\n    max_concurrency: 0\n    steps: [{jq: .}]\n",
        "max_concurrency",
    );
    assert_rejected(
        "steps:\n  - until: .\n    steps: [{jq: .}]\n",
        "requires 'max_iterations'",
    );
    assert_rejected(
        "steps:\n  - while: .\n    max_iterations: 0\n    steps: [{jq: .}]\n",
        "at least 1",
    );
    assert_rejected(
        "steps:\n  - while: .\n    until: .\n    max_iterations: 1\n    steps: [{jq: .}]\n",
        "found while and until",
    );
    assert_rejected(
        "steps:\n  - jq: .\n    steps: [{jq: .}]\n",
        "unknown field 'steps'; 'jq' steps accept",
    );
}

// --- placement rules ------------------------------------------------------

#[test]
fn break_must_be_inside_a_loop_body() {
    assert_rejected("steps:\n  - break: true\n", "'break'");
    assert_rejected(
        "steps:\n  - group:\n      - break: true\n",
        "must be inside",
    );
    parse(
        "steps:\n  - for_each: .\n    steps:\n      - switch:\n          - when: .\n            steps:\n              - break: true\n",
    );
    parse(
        "steps:\n  - until: .\n    max_iterations: 1\n    steps:\n      - jq: .\n        on_error:\n          - break: true\n",
    );
}

#[test]
fn break_and_stop_cannot_cross_concurrency() {
    assert_rejected(
        "steps:\n  - until: .\n    max_iterations: 1\n    steps:\n      - parallel:\n          a:\n            - break: true\n",
        "must be inside",
    );
    assert_rejected(
        "steps:\n  - parallel:\n      a:\n        - stop: true\n",
        "'stop'",
    );
    assert_rejected(
        "steps:\n  - for_each: .\n    max_concurrency: 2\n    steps:\n      - break: true\n",
        "must be inside",
    );
    assert_rejected(
        "steps:\n  - for_each: .\n    max_concurrency: 2\n    steps:\n      - stop: true\n",
        "'stop'",
    );
    parse(
        "steps:\n  - parallel:\n      a:\n        - until: .\n          max_iterations: 1\n          steps:\n            - break: true\n",
    );
    parse(
        "steps:\n  - for_each: .\n    max_concurrency: 2\n    steps:\n      - for_each: .\n        steps:\n          - break: true\n",
    );
}

#[test]
fn ask_cannot_run_concurrently() {
    assert_rejected(
        "steps:\n  - parallel:\n      a:\n        - ask: q\n",
        "'ask'",
    );
    assert_rejected(
        "steps:\n  - for_each: .\n    max_concurrency: 2\n    steps:\n      - group:\n          - ask: q\n",
        "'ask'",
    );
    parse("steps:\n  - for_each: .\n    steps:\n      - ask: q\n");
}

#[test]
fn a_fixed_path_write_cannot_run_in_concurrent_items() {
    assert_rejected(
        "steps:\n  - for_each: .\n    max_concurrency: 2\n    steps:\n      - write: out.txt\n",
        "fixed path",
    );
    assert_rejected(
        "steps:\n  - for_each: .\n    max_concurrency: 2\n    steps:\n      - for_each: .\n        steps:\n          - write: out.txt\n",
        "fixed path",
    );
    parse(
        "steps:\n  - for_each: .\n    max_concurrency: 2\n    steps:\n      - write: 'out-{{ loop.index }}.txt'\n",
    );
    parse(
        "steps:\n  - parallel:\n      a:\n        - write: a.txt\n      b:\n        - write: b.txt\n",
    );
    parse("steps:\n  - for_each: .\n    steps:\n      - write: out.txt\n");
}

// --- inputs ---------------------------------------------------------------

#[test]
fn validates_input_definitions() {
    assert_rejected(
        "inputs:\n  '1x': string\nsteps:\n  - jq: .\n",
        "invalid input",
    );
    assert_rejected(
        "inputs:\n  n: { type: integer, default: 'x' }\nsteps:\n  - jq: .\n",
        "inputs.n.default",
    );
    assert_rejected(
        "inputs:\n  n: 3\nsteps:\n  - jq: .\n",
        "expected a JSON Schema",
    );
    let wf = parse(
        "inputs:\n  any:\n  opt: { type: [string, 'null'], default: null }\nsteps:\n  - jq: .\n",
    );
    assert_eq!(wf.inputs[0].1.schema, serde_json::json!({}));
    assert_eq!(wf.inputs[1].1.default, Some(serde_json::Value::Null));
    assert!(wf.declares_inputs());
}

#[test]
fn only_a_plain_string_input_keeps_cli_text_verbatim() {
    let wf = parse(
        "inputs:\n  s: string\n  n: number\n  either: { type: [string, number] }\nsteps:\n  - jq: .\n",
    );
    let wants: Vec<bool> = wf
        .inputs
        .iter()
        .map(|(_, definition)| definition.wants_raw_string())
        .collect();
    assert_eq!(wants, [true, false, false]);
}

#[test]
fn parses_a_top_level_input_schema() {
    let wf = parse("schemas:\n  req: {type: object}\ninput_schema: req\nsteps:\n  - jq: .\n");
    assert_eq!(
        wf.input_schema.as_ref().unwrap().name.as_deref(),
        Some("req")
    );
    let wf = parse("input_schema: {type: array}\nsteps:\n  - jq: .\n");
    assert!(wf.input_schema.as_ref().unwrap().name.is_none());
    assert_rejected(
        "input_schema: missing\nsteps:\n  - jq: .\n",
        "schema 'missing', which is not defined",
    );
}

// --- helpers --------------------------------------------------------------

#[test]
fn labels_and_llm_classification() {
    let wf = parse("steps:\n  - id: named\n    prompt: x\n  - agent: a.md\n  - jq: .\n");
    assert_eq!(wf.steps[0].label_or(1), "named");
    assert_eq!(wf.steps[2].label_or(3), "step-3");
    assert_eq!(wf.steps[2].progress_label(7), "step-7 (jq)");
    assert!(wf.steps[0].calls_model());
    assert!(wf.steps[1].calls_model());
    assert!(!wf.steps[2].calls_model());
}

#[test]
fn looks_like_path_distinguishes_files_from_names() {
    assert!(looks_like_path("./a", &["md"]));
    assert!(looks_like_path("dir/a", &["md"]));
    assert!(looks_like_path("a.md", &["md"]));
    assert!(!looks_like_path("researcher", &["md"]));
    assert!(!looks_like_path("a.yml", &["md"]));
}

#[test]
fn defaults_fold_field_by_field() {
    let child = WorkflowDefaults {
        model: Some("child".into()),
        ..Default::default()
    };
    let parent = WorkflowDefaults {
        model: Some("parent".into()),
        temperature: Some(0.3),
        timeout: Some(9),
        ..Default::default()
    };
    let folded = WorkflowDefaults::fold(&[child, parent]);
    assert_eq!(folded.model.as_deref(), Some("child"));
    assert_eq!(folded.temperature, Some(0.3));
    assert_eq!(folded.timeout, Some(9));
}
