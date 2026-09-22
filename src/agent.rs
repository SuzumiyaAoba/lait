use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    async_io,
    config::ConfigFile,
    frontmatter, llm,
    reasoning::ReasoningEffort,
    report,
    schema::{self, JsonSchemaEntry},
};

/// An agent Markdown file: YAML frontmatter (model/reasoning defaults,
/// input/output schema, whether to request structured output) followed by a
/// Markdown body that is the system prompt template. Deserialized directly
/// from the frontmatter YAML — `system_prompt_template` is `#[serde(skip)]`
/// and filled in from the body afterward (see `parse_agent`) — rather than
/// through a separate frontmatter struct copied field-by-field into this
/// one, so the field list exists in exactly one place.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    pub(crate) structured_output: bool,
    pub(crate) schema_name: Option<String>,
    /// Names of `mcp_servers:` entries (from `lait.config.yml`) whose tools
    /// this agent may call.
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    /// Names of `skills:` entries (from `lait.config.yml`) whose content is
    /// appended to this agent's system prompt.
    pub(crate) skills: Option<Vec<String>>,
    /// Names of `agents:` entries (from `lait.config.yml`) made available as
    /// callable subagent tools during this agent's tool-call loop.
    pub(crate) subagents: Option<Vec<String>>,
    /// Names of `tools:` entries (from `lait.config.yml`) made available as
    /// callable shell-command tools during this agent's tool-call loop.
    pub(crate) tools: Option<Vec<String>>,
    /// The Markdown body, rendered as a handlebars template against the
    /// agent's input (see `crate::template::render`) to produce the system
    /// prompt actually sent to the model. Never present in the frontmatter
    /// YAML itself — see the struct doc above.
    #[serde(skip)]
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

/// Resolves `lait agent run`'s `FILE` argument: `argument` itself when it
/// exists as a file, else an `agents:` registry entry of that name — the
/// agent-side counterpart of `workflow::resolve_run_target`, same
/// file-wins-with-a-note shadowing rule and same already-absolute registry
/// paths (see its doc). The registry here also covers names `lait deps`
/// materialized into `.lait/deps/`, which `config::load` merges into
/// `agents:` before this ever runs.
pub(crate) fn resolve_run_target(argument: &Path, file_config: &ConfigFile) -> PathBuf {
    if argument.is_file() {
        if let Some(name) = argument.to_str()
            && file_config.agents.contains_key(name)
        {
            report::note(format_args!(
                "'{name}' exists as a file and is also an 'agents:' entry; running the file"
            ));
        }
        return argument.to_path_buf();
    }
    let Some(name) = argument.to_str() else {
        return argument.to_path_buf();
    };
    match file_config.agents.get(name) {
        // A dep-sourced entry resolves here too, so the note names the
        // registry rather than the file it happened to come from (a dep's
        // own `lait.deps.yml` isn't `lait.config.yml`).
        Some(resolved) => {
            report::note(format_args!(
                "resolved '{name}' to '{}' via 'agents:'",
                resolved.display(),
            ));
            resolved.clone()
        }
        None => argument.to_path_buf(),
    }
}

fn read_agent_context(path_or_name: impl std::fmt::Display) -> String {
    format!("failed to read agent file '{path_or_name}'")
}
fn parse_agent_context(path_or_name: impl std::fmt::Display) -> String {
    format!("failed to parse agent file '{path_or_name}'")
}

pub(crate) fn load_agent(path: &Path) -> Result<AgentFile> {
    let contents =
        async_io::read_to_string_sync(path).with_context(|| read_agent_context(path.display()))?;
    parse_agent(&contents).with_context(|| parse_agent_context(path.display()))
}

/// Loads an agent file through the cancellation-aware filesystem worker used
/// by timed workflow steps. The synchronous [`load_agent`] remains for local
/// commands (lint/init/top-level `agent run`) that do not have a step timeout.
pub(crate) async fn load_agent_cancellable(
    path: &Path,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<AgentFile> {
    let contents =
        async_io::read_to_string_cancellable(path, cancellation, async_io::MAX_READ_BYTES)
            .await
            .with_context(|| read_agent_context(path.display()))?;
    parse_agent(&contents).with_context(|| parse_agent_context(path.display()))
}

/// `pub(crate)` rather than private because `deps::ops` validates a fetched
/// agent file's bytes with it before the dependency is registered — the
/// same parse a later `agent run` would do, moved to the fetch boundary.
pub(crate) fn parse_agent(contents: &str) -> Result<AgentFile> {
    let (mut agent, body) = frontmatter::parse::<AgentFile>(contents, "agent file")?;

    if agent.structured_output && agent.output_schema.is_none() {
        bail!("'structured_output: true' requires an 'output_schema'");
    }
    if !agent.structured_output && agent.output_schema.is_some() {
        bail!("'output_schema' is set but 'structured_output' is not true");
    }
    llm::validate_sampling_params(
        agent.temperature,
        agent.top_p,
        agent.max_tokens,
        "the agent file",
    )?;
    llm::validate_max_tool_rounds(agent.max_tool_rounds, "the agent file")?;

    agent.system_prompt_template = body;
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::{load_agent, parse_agent};
    use crate::schema::JsonSchemaEntry;
    use serde_json::json;

    /// `load_agent` now reads through `async_io::read_to_string_sync` (see
    /// its call site's comment) rather than a bare `std::fs::read_to_string`
    /// — pins that the crate-wide 16MiB read limit applies here too. Mirrors
    /// `async_io::read_to_string_sync_rejects_a_file_beyond_max_read_bytes`.
    #[test]
    fn load_agent_rejects_a_file_beyond_max_read_bytes() {
        let path = crate::test_support::unique_temp_path("lait-agent-read-limit", ".md");
        std::fs::write(&path, vec![b'a'; crate::async_io::MAX_READ_BYTES + 1]).unwrap();

        let error = load_agent(&path).unwrap_err();
        assert!(
            format!("{error:#}").contains("read limit"),
            "error: {error:#}"
        );
        let _ = std::fs::remove_file(path);
    }

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
    fn rejects_a_literal_system_prompt_template_key_in_frontmatter() {
        // `system_prompt_template` is `#[serde(skip)]` on `AgentFile` (set
        // from the Markdown body after parsing, never from frontmatter YAML)
        // — confirms `deny_unknown_fields` still treats it as unknown rather
        // than silently accepting-and-discarding a frontmatter key that
        // happens to share the field's name.
        let result = parse_agent("---\nsystem_prompt_template: nope\n---\nbody\n");
        assert!(result.is_err());
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
