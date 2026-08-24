use std::{fs, path::Path};

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
    pub(crate) prompt: String,
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
