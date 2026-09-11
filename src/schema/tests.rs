use super::{
    SchemaKind, document_schema_json, unrecognized_type_names, validate_input_against_schema,
};
use serde_json::json;

#[test]
fn finds_no_unrecognized_types_in_an_ordinary_schema() {
    let schema = json!({
        "type": "object",
        "properties": {
            "age": {"type": "integer"},
            "tags": {"type": "array", "items": {"type": "string"}},
        },
    });
    assert!(unrecognized_type_names(&schema).is_empty());
}

#[test]
fn finds_an_unrecognized_top_level_type() {
    let schema = json!({"type": "sting"});
    assert_eq!(unrecognized_type_names(&schema), vec!["sting".to_owned()]);
}

#[test]
fn finds_an_unrecognized_type_nested_in_properties_and_items() {
    let schema = json!({
        "type": "object",
        "properties": {
            "age": {"type": "int"},
            "tags": {"type": "array", "items": {"type": "txt"}},
        },
    });
    let mut found = unrecognized_type_names(&schema);
    found.sort();
    assert_eq!(found, vec!["int".to_owned(), "txt".to_owned()]);
}

#[test]
fn finds_an_unrecognized_type_inside_an_array_of_types() {
    let schema = json!({"type": ["string", "nullish"]});
    assert_eq!(unrecognized_type_names(&schema), vec!["nullish".to_owned()]);
}

#[test]
fn deduplicates_a_repeated_unrecognized_type_name() {
    let schema = json!({
        "type": "object",
        "properties": {
            "a": {"type": "sting"},
            "b": {"type": "sting"},
        },
    });
    assert_eq!(unrecognized_type_names(&schema), vec!["sting".to_owned()]);
}

#[test]
fn accepts_an_object_with_every_required_field() {
    let schema = json!({"type": "object", "required": ["city"]});
    let input = json!({"city": "Tokyo", "extra": true});
    assert!(validate_input_against_schema(&schema, &input).is_ok());
}

#[test]
fn rejects_a_non_object_input() {
    let schema = json!({"type": "object", "required": ["city"]});
    assert!(validate_input_against_schema(&schema, &json!("Tokyo")).is_err());
}

#[test]
fn rejects_an_object_missing_a_required_field() {
    let schema = json!({"type": "object", "required": ["city", "population"]});
    let input = json!({"city": "Tokyo"});
    let error = validate_input_against_schema(&schema, &input).unwrap_err();
    assert!(error.to_string().contains("population"));
}

#[test]
fn accepts_any_object_when_the_schema_has_no_required_list() {
    let schema = json!({"type": "object"});
    assert!(validate_input_against_schema(&schema, &json!({})).is_ok());
}

#[test]
fn accepts_a_property_of_the_declared_type() {
    let schema = json!({
        "type": "object",
        "properties": {"age": {"type": "integer"}},
    });
    assert!(validate_input_against_schema(&schema, &json!({"age": 30})).is_ok());
}

#[test]
fn rejects_a_property_of_the_wrong_type() {
    let schema = json!({
        "type": "object",
        "properties": {"age": {"type": "integer"}},
    });
    let error = validate_input_against_schema(&schema, &json!({"age": "thirty"})).unwrap_err();
    assert!(error.to_string().contains("input.age"), "{error}");
    assert!(error.to_string().contains("integer"), "{error}");
}

#[test]
fn rejects_a_non_integer_number_for_an_integer_property() {
    let schema = json!({
        "type": "object",
        "properties": {"age": {"type": "integer"}},
    });
    assert!(validate_input_against_schema(&schema, &json!({"age": 30.5})).is_err());
}

#[test]
fn accepts_either_type_in_an_array_of_types() {
    let schema = json!({
        "type": "object",
        "properties": {"nickname": {"type": ["string", "null"]}},
    });
    assert!(validate_input_against_schema(&schema, &json!({"nickname": "Taro"})).is_ok());
    assert!(validate_input_against_schema(&schema, &json!({"nickname": null})).is_ok());
    assert!(validate_input_against_schema(&schema, &json!({"nickname": 1})).is_err());
}

#[test]
fn validates_a_nested_object_property() {
    let schema = json!({
        "type": "object",
        "properties": {
            "address": {
                "type": "object",
                "required": ["city"],
                "properties": {"city": {"type": "string"}},
            },
        },
    });
    assert!(validate_input_against_schema(&schema, &json!({"address": {"city": "Tokyo"}})).is_ok());

    let error = validate_input_against_schema(&schema, &json!({"address": {}})).unwrap_err();
    assert!(error.to_string().contains("input.address"), "{error}");
    assert!(error.to_string().contains("city"), "{error}");

    let error =
        validate_input_against_schema(&schema, &json!({"address": {"city": 1}})).unwrap_err();
    assert!(error.to_string().contains("input.address.city"), "{error}");
}

#[test]
fn validates_array_items_against_their_schema() {
    let schema = json!({
        "type": "object",
        "properties": {"tags": {"type": "array", "items": {"type": "string"}}},
    });
    assert!(validate_input_against_schema(&schema, &json!({"tags": ["a", "b"]})).is_ok());

    let error = validate_input_against_schema(&schema, &json!({"tags": ["a", 1]})).unwrap_err();
    assert!(error.to_string().contains("input.tags[1]"), "{error}");
}

#[test]
fn rejects_a_value_outside_an_enum() {
    let schema = json!({
        "type": "object",
        "properties": {"status": {"enum": ["open", "closed"]}},
    });
    assert!(validate_input_against_schema(&schema, &json!({"status": "open"})).is_ok());
    assert!(validate_input_against_schema(&schema, &json!({"status": "pending"})).is_err());
}

#[test]
fn ignores_extra_fields_not_declared_in_properties() {
    // A schema written for a Structured Outputs `output_schema` (strict
    // mode requires `additionalProperties: false`) must stay usable as an
    // `input_schema` without rejecting extra input fields.
    let schema = json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "additionalProperties": false,
    });
    assert!(validate_input_against_schema(&schema, &json!({"city": "Tokyo", "extra": 1})).is_ok());
}

// The three `lait schema` documents below are hand-written (see
// `document_schema_json`'s doc comment for why), so nothing at compile
// time keeps them from drifting away from what `config::ConfigFile`/
// `workflow::model::WorkflowFile`/`agent::AgentFile` actually accept.
// Every test below feeds the *same* YAML text (parsed once into
// `serde_json::Value` for the schema validator, and once through the
// real crate parser) to both, so a field this module's authors forget to
// mirror into the JSON Schema shows up as a test failure here rather
// than silently going stale.

fn compiled_schema(kind: SchemaKind) -> jsonschema::Validator {
    let document: serde_json::Value = serde_json::from_str(&document_schema_json(kind).unwrap())
        .expect("embedded schema document must be valid JSON");
    jsonschema::validator_for(&document).expect("embedded schema document must compile")
}

fn yaml_to_json(yaml: &str) -> serde_json::Value {
    serde_yaml::from_str(yaml).expect("fixture YAML must itself be well-formed")
}

/// A unique path under the OS temp directory for a real-parser fixture
/// (`workflow::load_workflow`/`agent::load_agent` both read from disk) —
/// same shape as `init.rs`'s own template tests.
fn temp_fixture_path(label: &str, extension: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "lait-schema-test-{label}-{}-{n}.{extension}",
        std::process::id()
    ))
}

#[test]
fn every_embedded_schema_compiles() {
    for kind in [SchemaKind::Workflow, SchemaKind::Config, SchemaKind::Agent] {
        compiled_schema(kind);
    }
}

const COMPREHENSIVE_WORKFLOW_YAML: &str = r#"
version: 1
name: sample
description: exercises most of the workflow vocabulary
default:
  model: local
  reasoning_effort: medium
  temperature: 0.5
  retry:
    max_attempts: 3
    delay_seconds: 1
    backoff: 2.0
  timeout: 30
  mcp: [fs]
  skills: [style]
  subagents: [helper]
  tools: [echo]
  workflow_timeout: 120
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
        api_key: sk-test
      model_id: my-model
      default_reasoning_effort: high
json_schemas:
  inline_example:
    schema:
      type: object
  file_example:
    file_path: ./schema.json
nodes:
  summarize:
    type: prompt
    model: local
    prompt: "summarize {{ input }}"
    system_prompt: "you are concise"
    files: ["./context.txt"]
    images: ["./picture.png"]
    input_schema: inline_example
    output_schema: inline_example
    schema_name: summary
    jq: ".summary"
    write_file: ./out.txt
    retry:
      max_attempts: 2
    timeout: 10
    mcp: [fs]
    max_tool_rounds: 4
    skills: [style]
    subagents: [helper]
    tools: [echo]
  delegate:
    type: agent
    agent: ./agents/researcher.md
    model: local
  sub:
    type: workflow
    workflow: ./sub.yml
  run_cmd:
    type: command
    command: ["echo", "{{ input }}"]
  reshape:
    type: transform
    jq: "."
  confirm:
    type: ask
    prompt: "continue?"
    choices: ["yes", "no"]
    default: "yes"
steps:
  - id: route
    switch:
      cases:
        - when: "true"
          steps:
            - use: summarize
      else:
        - use: reshape
  - id: fanout
    parallel:
      branches:
        - id: branch-a
          steps:
            - use: run_cmd
        - id: branch-b
          steps:
            - use: reshape
      join: "."
  - id: repeat
    loop:
      while: "false"
      max_iterations: 3
      steps:
        - use: reshape
  - id: each
    for_each:
      items: "[1, 2, 3]"
      max_concurrency: 2
      steps:
        - use: reshape
  - use: delegate
  - use: sub
  - use: confirm
  - stop: true
"#;

#[test]
fn workflow_schema_accepts_a_document_the_real_parser_accepts() {
    let path = temp_fixture_path("workflow-ok", "yml");
    std::fs::write(&path, COMPREHENSIVE_WORKFLOW_YAML).unwrap();
    let parsed = crate::workflow::load_workflow(&path);
    std::fs::remove_file(&path).ok();
    parsed.expect("fixture must be accepted by the real workflow parser");

    let validator = compiled_schema(SchemaKind::Workflow);
    let instance = yaml_to_json(COMPREHENSIVE_WORKFLOW_YAML);
    assert!(
        validator.is_valid(&instance),
        "schema rejected a document the real parser accepts: {:?}",
        validator.iter_errors(&instance).collect::<Vec<_>>()
    );
}

#[test]
fn workflow_schema_rejects_a_document_missing_required_steps() {
    let invalid = "name: no-steps\n";
    let validator = compiled_schema(SchemaKind::Workflow);
    assert!(!validator.is_valid(&yaml_to_json(invalid)));

    let path = temp_fixture_path("workflow-bad", "yml");
    std::fs::write(&path, invalid).unwrap();
    let parsed = crate::workflow::load_workflow(&path);
    std::fs::remove_file(&path).ok();
    assert!(
        parsed.is_err(),
        "the real workflow parser must also reject a file with no steps"
    );
}

const COMPREHENSIVE_CONFIG_YAML: &str = r#"
base_url: http://localhost:1234/v1
api_key: sk-test
default:
  model: local
  reasoning_effort: medium
  system: "you are concise"
  temperature: 0.5
  top_p: 0.9
  max_tokens: 512
  mcp: [fs]
  max_tool_rounds: 4
  skills: [style]
  subagents: [helper]
  tools: [echo]
  render: true
  history: true
  cache: true
  cache_ttl: 3600
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
        api_key: sk-test
      model_id: my-model
      default_reasoning_effort: high
      default_temperature: 0.7
mcp_servers:
  fs:
    command: npx
    args: ["-y", "server-fs"]
    env:
      TOKEN: abc
    allowed_tools: [read_file]
skills:
  style: ./skills/style.md
agents:
  helper: ./agents/helper.md
prompts:
  greet:
    template: "Hello {{ input }}"
    model: local
    vars:
      name: world
workflows:
  main: ./workflow.yml
tool_policy:
  allow: ["fs__*"]
  deny: ["fs__delete"]
tools:
  echo:
    description: echoes input
    command: ["echo", "{{ input.text }}"]
    parameters:
      type: object
      properties:
        text: { type: string }
    timeout: 5
"#;

#[test]
fn config_schema_accepts_a_document_the_real_parser_accepts() {
    let parsed: Result<crate::config::ConfigFile, _> =
        serde_yaml::from_str(COMPREHENSIVE_CONFIG_YAML);
    parsed.expect("fixture must be accepted by the real config parser");

    let validator = compiled_schema(SchemaKind::Config);
    let instance = yaml_to_json(COMPREHENSIVE_CONFIG_YAML);
    assert!(
        validator.is_valid(&instance),
        "schema rejected a document the real parser accepts: {:?}",
        validator.iter_errors(&instance).collect::<Vec<_>>()
    );
}

#[test]
fn config_schema_rejects_an_unknown_top_level_field() {
    let invalid = "not_a_real_field: 1\n";
    let validator = compiled_schema(SchemaKind::Config);
    assert!(!validator.is_valid(&yaml_to_json(invalid)));

    let parsed: Result<crate::config::ConfigFile, _> = serde_yaml::from_str(invalid);
    assert!(
        parsed.is_err(),
        "the real config parser must also reject an unknown top-level field"
    );
}

const COMPREHENSIVE_AGENT_FRONTMATTER_YAML: &str = r#"
name: sample-agent
description: exercises most of the agent frontmatter vocabulary
model: local
reasoning_effort: medium
temperature: 0.5
top_p: 0.9
max_tokens: 512
input_schema:
  schema:
    type: object
    required: [text]
output_schema:
  file_path: ./schema.json
structured_output: true
schema_name: summary
mcp: [fs]
max_tool_rounds: 4
skills: [style]
subagents: [helper]
tools: [echo]
"#;

#[test]
fn agent_schema_accepts_a_document_the_real_parser_accepts() {
    let mut file_contents = "---\n".to_owned();
    file_contents.push_str(COMPREHENSIVE_AGENT_FRONTMATTER_YAML.trim_start_matches('\n'));
    file_contents.push_str("---\n\nYou are a helpful assistant.\n");
    let path = temp_fixture_path("agent-ok", "md");
    std::fs::write(&path, &file_contents).unwrap();
    let parsed = crate::agent::load_agent(&path);
    std::fs::remove_file(&path).ok();
    parsed.expect("fixture must be accepted by the real agent parser");

    let validator = compiled_schema(SchemaKind::Agent);
    let instance = yaml_to_json(COMPREHENSIVE_AGENT_FRONTMATTER_YAML);
    assert!(
        validator.is_valid(&instance),
        "schema rejected a document the real parser accepts: {:?}",
        validator.iter_errors(&instance).collect::<Vec<_>>()
    );
}

#[test]
fn agent_schema_rejects_an_invalid_reasoning_effort() {
    let invalid = "reasoning_effort: extreme\n";
    let validator = compiled_schema(SchemaKind::Agent);
    assert!(!validator.is_valid(&yaml_to_json(invalid)));

    let parsed: Result<crate::agent::AgentFile, _> = serde_yaml::from_str(invalid);
    assert!(
        parsed.is_err(),
        "the real agent frontmatter parser must also reject an unknown reasoning_effort"
    );
}
#[test]
fn workflow_schema_and_parser_reject_invalid_control_shapes() {
    let validator = compiled_schema(SchemaKind::Workflow);
    for step in [
        "{}",
        "{stop: false}",
        "{break: false}",
        "{use: n, stop: true, break: true}",
        "{use: n, loop: {while: 'true', max_iterations: 1, steps: [{use: n}]}}",
        "{loop: {while: 'true', until: 'true', max_iterations: 1, steps: [{use: n}]}}",
        "{parallel: {branches: []}}",
        "{switch: {cases: []}}",
        "{use: n, on_error: {steps: []}}",
    ] {
        let source = format!("nodes:\n  n: {{type: transform, jq: '.'}}\nsteps: [{step}]\n");
        assert!(
            crate::workflow::parse_workflow(&source).is_err(),
            "parser accepted {step}"
        );
        assert!(
            !validator.is_valid(&yaml_to_json(&source)),
            "schema accepted {step}"
        );
    }
}
#[test]
fn published_schemas_reject_sampling_values_rejected_by_runtime() {
    for settings in [
        serde_json::json!({"temperature": -0.1}),
        serde_json::json!({"temperature": 2.1}),
        serde_json::json!({"top_p": -0.1}),
        serde_json::json!({"top_p": 1.1}),
        serde_json::json!({"max_tokens": 0}),
        serde_json::json!({"max_tool_rounds": 0}),
    ] {
        assert!(
            crate::llm::validate_sampling_params(
                settings
                    .get("temperature")
                    .and_then(serde_json::Value::as_f64),
                settings.get("top_p").and_then(serde_json::Value::as_f64),
                settings
                    .get("max_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .map(|v| v as u32),
                "schema fixture",
            )
            .and_then(|_| crate::llm::validate_max_tool_rounds(
                settings
                    .get("max_tool_rounds")
                    .and_then(serde_json::Value::as_u64)
                    .map(|v| v as usize),
                "schema fixture",
            ))
            .is_err()
        );
        for (kind, document) in [
            (SchemaKind::Agent, settings.clone()),
            (SchemaKind::Config, serde_json::json!({"default": settings})),
            (
                SchemaKind::Workflow,
                serde_json::json!({"default": settings, "steps": [{"stop": true}]}),
            ),
        ] {
            assert!(
                !compiled_schema(kind).is_valid(&document),
                "schema accepted {document}"
            );
        }
    }
}
#[test]
fn workflow_schema_and_parser_reject_incomplete_node_actions() {
    let validator = compiled_schema(SchemaKind::Workflow);
    for node in [
        "{type: prompt}",
        "{type: prompt, prompt: hi, schema_name: answer}",
        "{type: transform}",
        "{type: command, command: ['  ']}",
        "{type: ask, prompt: '  '}",
        "{type: ask, prompt: hi, choices: []}",
        "{type: ask, prompt: hi, choices: ['']}",
    ] {
        let source = format!("nodes:\n  n: {node}\nsteps: [{{use: n}}]\n");
        assert!(
            crate::workflow::parse_workflow(&source).is_err(),
            "parser accepted {node}"
        );
        assert!(
            !validator.is_valid(&yaml_to_json(&source)),
            "schema accepted {node}"
        );
    }
}

#[test]
fn agent_schema_rejects_inconsistent_structured_output_settings() {
    let validator = compiled_schema(SchemaKind::Agent);
    for source in [
        "structured_output: true\n",
        "output_schema: {schema: {type: object}}\n",
        "output_schema: {schema: {type: object}}\nstructured_output: false\n",
    ] {
        let path = temp_fixture_path("agent-structured-output", "md");
        std::fs::write(&path, format!("---\n{source}---\nPrompt")).unwrap();
        let parsed = crate::agent::load_agent(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(parsed.is_err(), "parser accepted {source}");
        assert!(
            !validator.is_valid(&yaml_to_json(source)),
            "schema accepted {source}"
        );
    }
}
#[test]
fn workflow_schema_matches_runtime_retry_and_deadline_constraints() {
    let validator = compiled_schema(SchemaKind::Workflow);
    for defaults in [
        "{workflow_timeout: 0}",
        "{retry: {}}",
        "{retry: {max_attempts: 1, backoff: -1}}",
    ] {
        let source = format!("default: {defaults}\nsteps: [{{stop: true}}]\n");
        assert!(crate::workflow::parse_workflow(&source).is_err());
        assert!(
            !validator.is_valid(&yaml_to_json(&source)),
            "schema accepted {source}"
        );
    }
}

#[test]
fn config_schema_rejects_ambiguous_authentication_sources() {
    let validator = compiled_schema(SchemaKind::Config);
    for source in [
        "api_key: literal\napi_key_cmd: echo key\n",
        "models:\n  n: [{model_id: n, provider: {base_url: 'http://localhost', api_key: literal, api_key_cmd: 'echo key'}}]\n",
    ] {
        let config = serde_yaml::from_str::<crate::config::ConfigFile>(source).unwrap();
        assert!(!crate::config::check_provider_api_key_sources(&config).is_empty());
        assert!(
            !validator.is_valid(&yaml_to_json(source)),
            "schema accepted {source}"
        );
    }
}

#[test]
fn config_schema_requires_exactly_one_mcp_transport() {
    let validator = compiled_schema(SchemaKind::Config);
    for server in ["{}", "{command: server, url: 'http://localhost'}"] {
        let source = format!("mcp_servers: {{server: {server}}}");
        let config = serde_yaml::from_str::<crate::config::ConfigFile>(&source).unwrap();
        assert!(
            config.mcp_servers["server"]
                .resolve_transport("server")
                .is_err()
        );
        assert!(
            !validator.is_valid(&yaml_to_json(&source)),
            "schema accepted {source}"
        );
    }
}
