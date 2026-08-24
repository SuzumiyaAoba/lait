use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{cli::ReasoningEffort, config::ModelMap};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowFile {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Model aliases usable by `model`/`steps[].model`, in the same shape as
    /// `lait.config.yml`'s top-level `models:`. Takes precedence over an alias of
    /// the same name defined in `lait.config.yml`.
    #[serde(default)]
    pub(crate) models: ModelMap,
    pub(crate) steps: Vec<StepDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepDefinition {
    pub(crate) id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// The prompt template sent to the model. A step without a `prompt` does not
    /// call the model at all; it must then have a `jq` filter, making it a
    /// data-only transformation step.
    pub(crate) prompt: Option<String>,
    /// Request a structured JSON response using the schema in this file, like the
    /// CLI's `--json-schema`. Requires `prompt`.
    pub(crate) json_schema: Option<PathBuf>,
    /// The name of the structured output schema. Defaults to `structured_output`,
    /// like the CLI's `--schema-name`. Only used together with `json_schema`.
    pub(crate) schema_name: Option<String>,
    /// A jq filter applied to this step's output (the model's response, or the
    /// running input if there is no `prompt`) before it becomes `{{ input }}` for
    /// the next step. The filtered value must be valid JSON.
    pub(crate) jq: Option<String>,
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
    for (index, step) in workflow.steps.iter().enumerate() {
        let label = || {
            step.id
                .clone()
                .unwrap_or_else(|| format!("step-{}", index + 1))
        };
        if step.prompt.is_none() && step.jq.is_none() {
            bail!(
                "step '{}' must have a 'prompt', a 'jq' filter, or both",
                label()
            );
        }
        if step.prompt.is_none() && step.json_schema.is_some() {
            bail!(
                "step '{}' has 'json_schema' but no 'prompt' to apply it to",
                label()
            );
        }
        if step.json_schema.is_none() && step.schema_name.is_some() {
            bail!("step '{}' has 'schema_name' but no 'json_schema'", label());
        }
    }
    Ok(workflow)
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

#[cfg(test)]
mod tests {
    use super::{parse_workflow, render_prompt};

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
        assert_eq!(workflow.model.as_deref(), Some("local"));
        assert_eq!(workflow.steps.len(), 2);
        assert_eq!(workflow.steps[0].id.as_deref(), Some("summarize"));
        assert_eq!(workflow.steps[1].model.as_deref(), Some("cloud"));
    }

    #[test]
    fn parses_workflow_with_embedded_models() {
        let workflow = parse_workflow(
            r#"
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
        assert_eq!(
            step.json_schema.as_deref().and_then(|p| p.to_str()),
            Some("schema.json")
        );
        assert_eq!(step.schema_name.as_deref(), Some("answer"));
        assert_eq!(step.jq.as_deref(), Some(".answer"));
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
}
