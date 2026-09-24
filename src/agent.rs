use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    async_io,
    config::ConfigFile,
    frontmatter, llm,
    reasoning::ReasoningEffort,
    report,
    schema::{self, SchemaSource},
};

/// An agent definition: model/sampling defaults, input/output schema,
/// capabilities, and a system prompt template. Read either from an agent
/// Markdown file — YAML frontmatter followed by a Markdown body that is the
/// system prompt template (see [`load_agent`]) — or from a workflow's
/// `agents:` map, where the same fields sit next to a `system:` template
/// (see [`AgentFile::finish_inline`]).
///
/// `output_schema` alone requests Structured Outputs; there is no separate
/// on/off switch. Relative `{file: ...}` schema paths are resolved against
/// the directory of the file the definition was written in.
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
    pub(crate) input_schema: Option<SchemaSource>,
    pub(crate) output_schema: Option<SchemaSource>,
    /// The Structured Outputs schema name sent with `output_schema`.
    /// Defaults to `structured_output`.
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
    /// The system prompt template of an inline (workflow `agents:`)
    /// definition. Rejected in an agent file's frontmatter, whose Markdown
    /// body is the template instead; moved into `system_prompt_template` by
    /// [`AgentFile::finish_inline`].
    system: Option<String>,
    /// The system prompt template, rendered as a handlebars template against
    /// the agent's input (see `crate::template::render_in`). Never present
    /// in YAML itself.
    #[serde(skip)]
    pub(crate) system_prompt_template: String,
}

impl AgentFile {
    /// Validates `input` against `input_schema`, when the agent declares one.
    pub(crate) fn validate_input(&self, input: &serde_json::Value) -> Result<()> {
        let Some(source) = &self.input_schema else {
            return Ok(());
        };
        let schema = schema::load_schema_value(source)?;
        schema::validate_value(&schema, input, "input")
    }

    /// The Structured Outputs schema name sent with `output_schema`.
    pub(crate) fn schema_name(&self) -> &str {
        self.schema_name.as_deref().unwrap_or("structured_output")
    }

    /// Completes an inline definition parsed from a workflow's `agents:`
    /// map: requires `system:` (the counterpart of an agent file's body),
    /// resolves relative schema files against `base_dir` (the workflow
    /// file's directory), and runs the same checks as [`load_agent`].
    pub(crate) fn finish_inline(&mut self, base_dir: &Path) -> Result<()> {
        let Some(system) = self.system.take() else {
            bail!("an inline agent requires a 'system:' prompt template");
        };
        self.system_prompt_template = system;
        self.finish(base_dir)
    }

    fn finish(&mut self, base_dir: &Path) -> Result<()> {
        llm::validate_sampling_params(
            self.temperature,
            self.top_p,
            self.max_tokens,
            "the agent definition",
        )?;
        llm::validate_max_tool_rounds(self.max_tool_rounds, "the agent definition")?;
        if let Some(name) = &self.schema_name {
            if self.output_schema.is_none() {
                bail!("'schema_name' requires an 'output_schema'");
            }
            schema::validate_schema_name(name)?;
        }
        for source in [&mut self.input_schema, &mut self.output_schema]
            .into_iter()
            .flatten()
        {
            source.resolve_relative_to(base_dir);
        }
        Ok(())
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
    parse_agent(&contents, base_dir_of(path)).with_context(|| parse_agent_context(path.display()))
}

/// Loads an agent file through the cancellation-aware filesystem worker used
/// by timed workflow steps. The synchronous [`load_agent`] remains for local
/// commands (lint/init/dry-run) that do not have a step timeout.
pub(crate) async fn load_agent_cancellable(
    path: &Path,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<AgentFile> {
    let contents =
        async_io::read_to_string_cancellable(path, cancellation, async_io::MAX_READ_BYTES)
            .await
            .with_context(|| read_agent_context(path.display()))?;
    parse_agent(&contents, base_dir_of(path)).with_context(|| parse_agent_context(path.display()))
}

fn base_dir_of(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// `pub(crate)` rather than private because `deps::ops` validates a fetched
/// agent file's bytes with it before the dependency is registered — the
/// same parse a later `agent run` would do, moved to the fetch boundary.
/// `base_dir` is what relative `{file: ...}` schema paths resolve against.
pub(crate) fn parse_agent(contents: &str, base_dir: &Path) -> Result<AgentFile> {
    let (mut agent, body) = frontmatter::parse::<AgentFile>(contents, "agent file")?;
    if agent.system.is_some() {
        bail!(
            "'system' is not a frontmatter field; an agent file's Markdown body is its system prompt"
        );
    }
    agent.system_prompt_template = body;
    agent.finish(base_dir)?;
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::{AgentFile, load_agent, parse_agent};
    use crate::schema::SchemaSource;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn parse(contents: &str) -> anyhow::Result<AgentFile> {
        parse_agent(contents, Path::new("/agents"))
    }

    /// `load_agent` reads through `async_io::read_to_string_sync` rather than
    /// a bare `std::fs::read_to_string` — pins that the crate-wide 16MiB read
    /// limit applies here too. Mirrors
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
        let agent = parse(
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
        assert!(agent.output_schema.is_none());
    }

    #[test]
    fn parses_inline_and_file_schemas_resolving_files_against_the_agent_directory() {
        let agent = parse(
            r#"---
input_schema:
  type: object
  required: [text]
output_schema:
  file: schemas/city.json
schema_name: city_fact
---
{{ input.text }}
"#,
        )
        .expect("agent should parse");

        assert_eq!(agent.schema_name(), "city_fact");
        assert!(matches!(agent.input_schema, Some(SchemaSource::Inline(_))));
        assert_eq!(
            agent.output_schema,
            Some(SchemaSource::File(PathBuf::from(
                "/agents/schemas/city.json"
            )))
        );
    }

    #[test]
    fn defaults_the_schema_name_to_structured_output() {
        let agent = parse("---\n---\nbody\n").expect("agent should parse");
        assert_eq!(agent.schema_name(), "structured_output");
    }

    #[test]
    fn rejects_a_schema_name_without_an_output_schema_or_with_invalid_characters() {
        assert!(parse("---\nschema_name: x\n---\nbody\n").is_err());
        assert!(
            parse("---\noutput_schema: {type: object}\nschema_name: \"bad name!\"\n---\nbody\n")
                .is_err()
        );
    }

    #[test]
    fn rejects_the_removed_structured_output_switch() {
        assert!(parse("---\nstructured_output: true\n---\nbody\n").is_err());
    }

    #[test]
    fn rejects_a_file_without_a_leading_frontmatter_delimiter() {
        assert!(parse("no frontmatter here\n").is_err());
    }

    #[test]
    fn rejects_a_file_with_unterminated_frontmatter() {
        assert!(parse("---\nname: agent\nbody without closing delimiter\n").is_err());
    }

    #[test]
    fn rejects_system_or_a_literal_template_key_in_frontmatter() {
        assert!(parse("---\nsystem: nope\n---\nbody\n").is_err());
        assert!(parse("---\nsystem_prompt_template: nope\n---\nbody\n").is_err());
    }

    #[test]
    fn validates_input_against_the_declared_input_schema() {
        let agent = parse(
            "---\ninput_schema:\n  type: object\n  required: [city]\n---\n{{ input.city }}\n",
        )
        .expect("agent should parse");

        assert!(agent.validate_input(&json!({"city": "Tokyo"})).is_ok());
        assert!(agent.validate_input(&json!({"other": true})).is_err());
        assert!(agent.validate_input(&json!("Tokyo")).is_err());
    }

    #[test]
    fn skips_input_validation_when_no_input_schema_is_declared() {
        let agent = parse("---\n---\n{{ input }}\n").expect("agent should parse");
        assert!(agent.validate_input(&json!("anything")).is_ok());
    }

    #[test]
    fn parses_temperature_top_p_and_max_tokens() {
        let agent =
            parse("---\nmodel: local\ntemperature: 0.7\ntop_p: 0.9\nmax_tokens: 256\n---\nbody\n")
                .expect("agent should parse");

        assert_eq!(agent.temperature, Some(0.7));
        assert_eq!(agent.top_p, Some(0.9));
        assert_eq!(agent.max_tokens, Some(256));
    }

    #[test]
    fn rejects_an_out_of_range_temperature() {
        assert!(parse("---\ntemperature: 2.5\n---\nbody\n").is_err());
    }

    #[test]
    fn parses_subagents() {
        let agent =
            parse("---\nsubagents: [researcher, writer]\n---\nbody\n").expect("agent should parse");
        assert_eq!(
            agent.subagents.as_deref(),
            Some(["researcher".to_owned(), "writer".to_owned()].as_slice())
        );
    }

    #[test]
    fn an_inline_definition_requires_system_and_resolves_schema_files() {
        let mut agent: AgentFile =
            serde_yaml::from_str("model: local\noutput_schema: {file: out.json}\nsystem: hi\n")
                .unwrap();
        agent.finish_inline(Path::new("/wf")).unwrap();
        assert_eq!(agent.system_prompt_template, "hi");
        assert_eq!(
            agent.output_schema,
            Some(SchemaSource::File(PathBuf::from("/wf/out.json")))
        );

        let mut missing: AgentFile = serde_yaml::from_str("model: local\n").unwrap();
        assert!(missing.finish_inline(Path::new("/wf")).is_err());
    }
}
