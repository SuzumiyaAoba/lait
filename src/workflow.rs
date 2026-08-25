use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    cli::ReasoningEffort,
    config::{DefaultSettings, ModelMap},
    jq,
    schema::JsonSchemaMap,
    template,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowFile {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) default: DefaultSettings,
    /// Model aliases usable by `default.model`/`steps[].model`, in the same shape as
    /// `lait.config.yml`'s top-level `models:`. Takes precedence over an alias of
    /// the same name defined in `lait.config.yml`.
    #[serde(default)]
    pub(crate) models: ModelMap,
    /// Named schema definitions usable by `steps[].json_schema`, each either a
    /// `file_path:` to a JSON schema file or an inline `schema:` body.
    #[serde(default)]
    pub(crate) json_schemas: JsonSchemaMap,
    pub(crate) steps: Vec<StepDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepDefinition {
    pub(crate) id: Option<String>,
    /// A jq filter evaluated against the current input (JSON-parsed, falling
    /// back to a JSON string for plain text, like `template::parse_input`).
    /// A falsy result (`false`/`null`) skips this step entirely, passing the
    /// input through unchanged to the next step. Mutually exclusive with
    /// `switch`.
    pub(crate) when: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// The prompt template sent to the model. A step without a `prompt` and
    /// without an `agent` does not call the model at all; it must then have a
    /// `jq` filter, making it a data-only transformation step. Mutually
    /// exclusive with `agent`.
    pub(crate) prompt: Option<String>,
    /// Path to an agent Markdown file (see `agent::load_agent`) whose system
    /// prompt, model/reasoning defaults, and input/output schema drive this
    /// step instead of `prompt`/`json_schema`/`schema_name`. Mutually
    /// exclusive with `prompt`, `json_schema`, and `schema_name`.
    pub(crate) agent: Option<PathBuf>,
    /// Request a structured JSON response using the named schema, like the CLI's
    /// `--json-schema`. Resolved against the workflow's top-level `json_schemas:`
    /// first; if no such key exists, treated as a path to a JSON schema file
    /// instead. Requires `prompt`.
    pub(crate) json_schema: Option<String>,
    /// The name of the structured output schema. Defaults to `structured_output`,
    /// like the CLI's `--schema-name`. Only used together with `json_schema`.
    pub(crate) schema_name: Option<String>,
    /// A jq filter applied to this step's output (the model's response, or the
    /// running input if there is no `prompt`) before it becomes `{{ input }}` for
    /// the next step. The filtered value must be valid JSON.
    pub(crate) jq: Option<String>,
    /// Turns this step into a branch router: evaluates `cases` in order and
    /// runs the first one whose `when` is truthy (or `else`, if none match).
    /// Mutually exclusive with every other field except `id`.
    pub(crate) switch: Option<SwitchDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SwitchDefinition {
    /// Evaluated in order; the first case whose `when` is truthy runs.
    pub(crate) cases: Vec<CaseDefinition>,
    /// Runs when no `case` matched. Required unless the workflow author is
    /// sure `cases` is exhaustive: a `switch` with no matching case and no
    /// `else` is a runtime error rather than a silent pass-through.
    #[serde(rename = "else")]
    pub(crate) else_steps: Option<Vec<StepDefinition>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaseDefinition {
    /// An optional label used only in progress output (like `StepDefinition::id`).
    pub(crate) id: Option<String>,
    /// A jq filter evaluated against the current input; see `StepDefinition::when`.
    pub(crate) when: String,
    pub(crate) steps: Vec<StepDefinition>,
}

pub(crate) fn load_workflow(path: &Path) -> Result<WorkflowFile> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read workflow file '{}'", path.display()))?;
    parse_workflow(&contents)
        .with_context(|| format!("failed to parse workflow file '{}'", path.display()))
}

fn parse_workflow(contents: &str) -> Result<WorkflowFile> {
    let workflow: WorkflowFile = serde_yaml::from_str(contents)?;
    if workflow.steps.is_empty() {
        bail!("workflow must contain at least one step");
    }
    validate_steps(&workflow.steps)?;
    Ok(workflow)
}

fn validate_steps(steps: &[StepDefinition]) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let label = || {
            step.id
                .clone()
                .unwrap_or_else(|| format!("step-{}", index + 1))
        };

        if let Some(switch) = &step.switch {
            let has_action_fields = step.when.is_some()
                || step.model.is_some()
                || step.reasoning_effort.is_some()
                || step.prompt.is_some()
                || step.agent.is_some()
                || step.json_schema.is_some()
                || step.schema_name.is_some()
                || step.jq.is_some();
            if has_action_fields {
                bail!(
                    "step '{}' has 'switch' set; it cannot also have 'when', 'model', \
                     'reasoning_effort', 'prompt', 'agent', 'json_schema', 'schema_name', or 'jq'",
                    label()
                );
            }
            if switch.cases.is_empty() {
                bail!("step '{}' has 'switch' with an empty 'cases' list", label());
            }
            for case in &switch.cases {
                if case.steps.is_empty() {
                    bail!(
                        "step '{}' has a 'switch' case with an empty 'steps' list",
                        label()
                    );
                }
                validate_steps(&case.steps)?;
            }
            if let Some(else_steps) = &switch.else_steps {
                if else_steps.is_empty() {
                    bail!(
                        "step '{}' has a 'switch' with an empty 'else' list",
                        label()
                    );
                }
                validate_steps(else_steps)?;
            }
            continue;
        }

        let calls_model = step.prompt.is_some() || step.agent.is_some();
        if !calls_model && step.jq.is_none() {
            bail!(
                "step '{}' must have a 'prompt', an 'agent', a 'jq' filter, a 'switch', or a combination",
                label()
            );
        }
        if step.prompt.is_some() && step.agent.is_some() {
            bail!("step '{}' cannot have both 'prompt' and 'agent'", label());
        }
        if step.agent.is_some() && (step.json_schema.is_some() || step.schema_name.is_some()) {
            bail!(
                "step '{}' has 'agent' set; 'json_schema'/'schema_name' come from the agent file and must not be set on the step",
                label()
            );
        }
        if !calls_model && step.json_schema.is_some() {
            bail!(
                "step '{}' has 'json_schema' but no 'prompt'/'agent' to apply it to",
                label()
            );
        }
        if step.json_schema.is_none() && step.schema_name.is_some() {
            bail!("step '{}' has 'schema_name' but no 'json_schema'", label());
        }
    }
    Ok(())
}

/// Replaces `{{ input }}` placeholders in a step's prompt template with `input`.
pub(crate) fn render_prompt(template: &str, input: &str) -> Result<String> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(relative_end) = rest[start..].find("}}") else {
            bail!("unterminated placeholder in prompt template: {template:?}");
        };
        let end = start + relative_end;
        rendered.push_str(&rest[..start]);
        let name = rest[start + 2..end].trim();
        match name {
            "input" => rendered.push_str(input),
            _ => bail!("unknown placeholder '{{{{ {name} }}}}' in prompt template"),
        }
        rest = &rest[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

/// Evaluates a `when`/case-condition jq filter against the current input,
/// using the same JSON-or-string coercion as `{{ input }}` templates
/// (`template::parse_input`) so a `when:` right after a plain-text `prompt:`
/// step doesn't fail just because the input isn't JSON.
pub(crate) fn eval_when(filter: &str, current_input: &str) -> Result<bool> {
    let value = template::parse_input(current_input);
    let input_json = serde_json::to_string(&value)
        .context("failed to serialize the current input for a 'when' condition")?;
    jq::apply_bool(filter, &input_json).context("failed to evaluate 'when' condition")
}

#[cfg(test)]
mod tests {
    use super::{eval_when, parse_workflow, render_prompt};
    use crate::schema::JsonSchemaEntry;

    #[test]
    fn renders_input_placeholder_with_and_without_spaces() {
        assert_eq!(
            render_prompt("summarize: {{input}}", "hello").unwrap(),
            "summarize: hello"
        );
        assert_eq!(
            render_prompt("summarize: {{ input }}", "hello").unwrap(),
            "summarize: hello"
        );
        assert_eq!(
            render_prompt("{{ input }} and {{ input }}", "x").unwrap(),
            "x and x"
        );
        assert_eq!(
            render_prompt("no placeholder here", "x").unwrap(),
            "no placeholder here"
        );
    }

    #[test]
    fn rejects_unknown_placeholder() {
        assert!(render_prompt("{{ nope }}", "x").is_err());
    }

    #[test]
    fn rejects_unterminated_placeholder() {
        assert!(render_prompt("{{ input", "x").is_err());
    }

    #[test]
    fn parses_workflow_with_multiple_steps() {
        let workflow = parse_workflow(
            r#"
name: example
description: summarize then translate
default:
  model: local
steps:
  - id: summarize
    prompt: "summarize: {{ input }}"
  - id: translate
    model: cloud
    reasoning_effort: high
    prompt: "translate: {{ input }}"
"#,
        )
        .expect("workflow should parse");

        assert_eq!(workflow.name.as_deref(), Some("example"));
        assert_eq!(workflow.default.model.as_deref(), Some("local"));
        assert_eq!(workflow.steps.len(), 2);
        assert_eq!(workflow.steps[0].id.as_deref(), Some("summarize"));
        assert_eq!(workflow.steps[1].model.as_deref(), Some("cloud"));
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
steps:
  - prompt: "{{ input }}"
  - model: cloud
    prompt: "{{ input }}"
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
    fn parses_a_step_with_json_schema_and_jq() {
        let workflow = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    json_schema: schema.json
    schema_name: answer
    jq: ".answer"
"#,
        )
        .expect("workflow with json_schema and jq should parse");

        let step = &workflow.steps[0];
        assert_eq!(step.json_schema.as_deref(), Some("schema.json"));
        assert_eq!(step.schema_name.as_deref(), Some("answer"));
        assert_eq!(step.jq.as_deref(), Some(".answer"));
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
steps:
  - prompt: "{{ input }}"
    json_schema: answer
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
        assert_eq!(workflow.steps[0].json_schema.as_deref(), Some("answer"));
    }

    #[test]
    fn parses_a_workflow_with_file_path_json_schemas() {
        let workflow = parse_workflow(
            r#"
json_schemas:
  answer:
    file_path: schema.json
steps:
  - prompt: "{{ input }}"
    json_schema: answer
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
steps:
  - prompt: "{{ input }}"
    json_schema: answer
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
steps:
  - prompt: "{{ input }}"
    json_schema: answer
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn allows_a_transform_only_step_with_no_prompt() {
        let workflow = parse_workflow(
            r#"
steps:
  - jq: ".answer"
"#,
        )
        .expect("a jq-only step should parse");

        assert!(workflow.steps[0].prompt.is_none());
        assert_eq!(workflow.steps[0].jq.as_deref(), Some(".answer"));
    }

    #[test]
    fn rejects_a_step_with_neither_prompt_nor_jq() {
        assert!(parse_workflow("steps:\n  - id: empty\n").is_err());
    }

    #[test]
    fn rejects_json_schema_without_a_prompt() {
        let result = parse_workflow("steps:\n  - jq: \".\"\n    json_schema: schema.json\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_schema_name_without_json_schema() {
        let result =
            parse_workflow("steps:\n  - prompt: \"{{ input }}\"\n    schema_name: answer\n");
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_step_with_an_agent() {
        let workflow = parse_workflow(
            r#"
steps:
  - agent: agents/extract.md
    jq: ".city"
"#,
        )
        .expect("workflow with an agent step should parse");

        assert_eq!(
            workflow.steps[0].agent.as_deref().and_then(|p| p.to_str()),
            Some("agents/extract.md")
        );
    }

    #[test]
    fn rejects_a_step_with_both_prompt_and_agent() {
        let result =
            parse_workflow("steps:\n  - prompt: \"{{ input }}\"\n    agent: agents/extract.md\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_step_with_agent_and_json_schema() {
        let result =
            parse_workflow("steps:\n  - agent: agents/extract.md\n    json_schema: schema.json\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_step_with_agent_and_schema_name() {
        let result =
            parse_workflow("steps:\n  - agent: agents/extract.md\n    schema_name: answer\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let result = parse_workflow(
            r#"
unexpected: true
steps:
  - prompt: "{{ input }}"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_step_with_a_when_guard() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: maybe
    when: '. != null'
    prompt: "{{ input }}"
"#,
        )
        .expect("workflow with a 'when' guard should parse");

        assert_eq!(workflow.steps[0].when.as_deref(), Some(". != null"));
    }

    #[test]
    fn parses_a_switch_with_cases_and_else() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: route
    switch:
      cases:
        - id: high
          when: '.severity == "high"'
          steps:
            - prompt: "escalate: {{ input }}"
        - when: '.severity == "medium"'
          steps:
            - prompt: "reply: {{ input }}"
      else:
        - jq: ".summary"
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
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
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
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
      else: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_switch_combined_with_prompt() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_switch_combined_with_when() {
        let result = parse_workflow(
            r#"
steps:
  - when: 'true'
    switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
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
            - prompt: "{{ input }}"
              agent: agents/extract.md
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn eval_when_coerces_plain_text_input_to_a_json_string() {
        assert!(eval_when(". == \"hello\"", "hello").unwrap());
        assert!(!eval_when(". == \"hello\"", "world").unwrap());
    }

    #[test]
    fn eval_when_evaluates_against_parsed_json_input() {
        assert!(eval_when(".flag", r#"{"flag":true}"#).unwrap());
        assert!(!eval_when(".flag", r#"{"flag":false}"#).unwrap());
    }
}
