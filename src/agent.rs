use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    async_io,
    cli::ReasoningEffort,
    frontmatter, llm,
    schema::{self, JsonSchemaEntry},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFrontmatter {
    name: Option<String>,
    description: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<u32>,
    input_schema: Option<JsonSchemaEntry>,
    output_schema: Option<JsonSchemaEntry>,
    #[serde(default)]
    structured_output: bool,
    schema_name: Option<String>,
    /// Names of `mcp_servers:` entries (from `lait.config.yml`) whose tools
    /// this agent may call.
    mcp: Option<Vec<String>>,
    max_tool_rounds: Option<usize>,
    /// Names of `skills:` entries (from `lait.config.yml`) whose content is
    /// appended to this agent's system prompt.
    skills: Option<Vec<String>>,
    /// Names of `agents:` entries (from `lait.config.yml`) made available as
    /// callable subagent tools during this agent's tool-call loop.
    subagents: Option<Vec<String>>,
}

/// An agent Markdown file: YAML frontmatter (model/reasoning defaults,
/// input/output schema, whether to request structured output) followed by a
/// Markdown body that is the system prompt template.
#[derive(Debug)]
pub(crate) struct AgentFile {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) input_schema: Option<JsonSchemaEntry>,
    pub(crate) output_schema: Option<JsonSchemaEntry>,
    pub(crate) structured_output: bool,
    pub(crate) schema_name: Option<String>,
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Option<Vec<String>>,
    pub(crate) subagents: Option<Vec<String>>,
    /// The Markdown body, rendered as a handlebars template against the
    /// agent's input (see `crate::template::render`) to produce the system
    /// prompt actually sent to the model.
    pub(crate) system_prompt_template: String,
}

impl AgentFile {
    /// Validates `input` against `input_schema`, when the agent declares one.
    /// A no-op when the agent has no `input_schema`.
    pub(crate) fn validate_input(&self, input: &serde_json::Value) -> Result<()> {
        let Some(entry) = &self.input_schema else {
            return Ok(());
        };
        let schema = schema::load_schema_value(entry)?;
        schema::validate_input_against_schema(&schema, input)
    }

    /// The name to use for the structured output schema, once `structured_output`
    /// is confirmed to be set.
    pub(crate) fn schema_name(&self) -> &str {
        self.schema_name.as_deref().unwrap_or("structured_output")
    }
}

pub(crate) fn load_agent(path: &Path) -> Result<AgentFile> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read agent file '{}'", path.display()))?;
    parse_agent(&contents)
        .with_context(|| format!("failed to parse agent file '{}'", path.display()))
}

/// Loads an agent file through the cancellation-aware filesystem worker used
/// by timed workflow steps. The synchronous [`load_agent`] remains for local
/// commands (lint/init/top-level `agent run`) that do not have a step timeout.
pub(crate) async fn load_agent_cancellable(
    path: &Path,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<AgentFile> {
    let error_path = path.to_owned();
    let wait_for_fifo_writer = cancellation.is_some();
    let contents = async_io::run_blocking(
        move |cancelled| {
            if wait_for_fifo_writer {
                async_io::read_to_string_wait_for_fifo_writer(
                    &error_path,
                    cancelled,
                    async_io::MAX_READ_BYTES,
                )
            } else {
                async_io::read_to_string(&error_path, cancelled, async_io::MAX_READ_BYTES)
            }
        },
        cancellation,
    )
    .await
    .with_context(|| format!("failed to read agent file '{}'", path.display()))?;
    parse_agent(&contents)
        .with_context(|| format!("failed to parse agent file '{}'", path.display()))
}

fn parse_agent(contents: &str) -> Result<AgentFile> {
    let (frontmatter, body) = frontmatter::split(contents, "agent file")?;
    let frontmatter: AgentFrontmatter =
        serde_yaml::from_str(frontmatter).context("failed to parse frontmatter")?;

    if frontmatter.structured_output && frontmatter.output_schema.is_none() {
        bail!("'structured_output: true' requires an 'output_schema'");
    }
    if !frontmatter.structured_output && frontmatter.output_schema.is_some() {
        bail!("'output_schema' is set but 'structured_output' is not true");
    }
    llm::validate_sampling_params(
        frontmatter.temperature,
        frontmatter.top_p,
        frontmatter.max_tokens,
        "the agent file",
    )?;
    llm::validate_max_tool_rounds(frontmatter.max_tool_rounds, "the agent file")?;

    Ok(AgentFile {
        name: frontmatter.name,
        description: frontmatter.description,
        model: frontmatter.model,
        reasoning_effort: frontmatter.reasoning_effort,
        temperature: frontmatter.temperature,
        top_p: frontmatter.top_p,
        max_tokens: frontmatter.max_tokens,
        input_schema: frontmatter.input_schema,
        output_schema: frontmatter.output_schema,
        structured_output: frontmatter.structured_output,
        schema_name: frontmatter.schema_name,
        mcp: frontmatter.mcp,
        max_tool_rounds: frontmatter.max_tool_rounds,
        skills: frontmatter.skills,
        subagents: frontmatter.subagents,
        system_prompt_template: body.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_agent;
    use crate::schema::JsonSchemaEntry;
    use serde_json::json;

    #[test]
    fn parses_frontmatter_and_body() {
        let agent = parse_agent(
            "---\nname: city-fact\ndescription: extracts a city fact\nmodel: local\nreasoning_effort: medium\n---\nExtract the city.\n{{ input.text }}\n",
        )
        .expect("agent should parse");

        assert_eq!(agent.name.as_deref(), Some("city-fact"));
        assert_eq!(agent.description.as_deref(), Some("extracts a city fact"));
        assert_eq!(agent.model.as_deref(), Some("local"));
        assert_eq!(
            agent.system_prompt_template,
            "Extract the city.\n{{ input.text }}"
        );
        assert!(!agent.structured_output);
        assert!(agent.output_schema.is_none());
    }

    #[test]
    fn parses_an_inline_input_and_output_schema_with_structured_output() {
        let agent = parse_agent(
            r#"---
input_schema:
  schema:
    type: object
    required: [text]
output_schema:
  schema:
    type: object
    required: [city]
structured_output: true
schema_name: city_fact
---
{{ input.text }}
"#,
        )
        .expect("agent should parse");

        assert!(agent.structured_output);
        assert_eq!(agent.schema_name(), "city_fact");
        match agent.input_schema {
            Some(JsonSchemaEntry::Inline { .. }) => {}
            _ => panic!("expected an inline input schema"),
        }
        match agent.output_schema {
            Some(JsonSchemaEntry::Inline { .. }) => {}
            _ => panic!("expected an inline output schema"),
        }
    }

    #[test]
    fn defaults_the_schema_name_to_structured_output() {
        let agent = parse_agent("---\n---\nbody\n").expect("agent should parse");
        assert_eq!(agent.schema_name(), "structured_output");
    }

    #[test]
    fn rejects_a_file_without_a_leading_frontmatter_delimiter() {
        assert!(parse_agent("no frontmatter here\n").is_err());
    }

    #[test]
    fn rejects_a_file_with_unterminated_frontmatter() {
        assert!(parse_agent("---\nname: agent\nbody without closing delimiter\n").is_err());
    }

    #[test]
    fn rejects_structured_output_true_without_an_output_schema() {
        let result = parse_agent("---\nstructured_output: true\n---\nbody\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_an_output_schema_without_structured_output() {
        let result = parse_agent("---\noutput_schema:\n  schema:\n    type: object\n---\nbody\n");
        assert!(result.is_err());
    }

    #[test]
    fn validates_input_against_the_declared_input_schema() {
        let agent = parse_agent(
            "---\ninput_schema:\n  schema:\n    type: object\n    required: [city]\n---\n{{ input.city }}\n",
        )
        .expect("agent should parse");

        assert!(agent.validate_input(&json!({"city": "Tokyo"})).is_ok());
        assert!(agent.validate_input(&json!({"other": true})).is_err());
    }

    #[test]
    fn skips_input_validation_when_no_input_schema_is_declared() {
        let agent = parse_agent("---\n---\n{{ input }}\n").expect("agent should parse");
        assert!(agent.validate_input(&json!("anything")).is_ok());
    }

    #[test]
    fn parses_temperature_top_p_and_max_tokens() {
        let agent = parse_agent(
            "---\nmodel: local\ntemperature: 0.7\ntop_p: 0.9\nmax_tokens: 256\n---\nbody\n",
        )
        .expect("agent should parse");

        assert_eq!(agent.temperature, Some(0.7));
        assert_eq!(agent.top_p, Some(0.9));
        assert_eq!(agent.max_tokens, Some(256));
    }

    #[test]
    fn rejects_an_out_of_range_temperature() {
        let result = parse_agent("---\ntemperature: 2.5\n---\nbody\n");
        assert!(result.is_err());
    }

    #[test]
    fn parses_subagents() {
        let agent = parse_agent("---\nsubagents: [researcher, writer]\n---\nbody\n")
            .expect("agent should parse");
        assert_eq!(
            agent.subagents.as_deref(),
            Some(["researcher".to_owned(), "writer".to_owned()].as_slice())
        );
    }

    #[test]
    fn leaves_subagents_unset_by_default() {
        let agent = parse_agent("---\n---\nbody\n").expect("agent should parse");
        assert!(agent.subagents.is_none());
    }
}
