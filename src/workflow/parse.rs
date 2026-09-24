//! Parses a workflow file (schema version 2) into the validated
//! [`WorkflowFile`] shape. Every static rule is checked here, before any
//! step runs: the one-kind-per-step shape, field applicability, id
//! uniqueness, schema/agent references, jq/template syntax, numeric bounds,
//! and where `break`/`stop`/`ask`/`write` may appear.

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    agent::AgentFile,
    config::ModelMap,
    jq, llm,
    schema::{self, SchemaMap, SchemaSource},
    template,
};

use super::model::*;

/// The keys that select a step's kind. Exactly one must be present.
pub(crate) const KIND_KEYS: [&str; 15] = [
    "prompt", "agent", "run", "workflow", "jq", "ask", "write", "group", "switch", "parallel",
    "for_each", "while", "until", "stop", "break",
];

/// The fields every step (except `stop`/`break`, which take no
/// `retry`/`timeout`/`on_error`) may carry next to its kind key.
const COMMON_KEYS: [&str; 7] = [
    "id", "when", "input", "output", "retry", "timeout", "on_error",
];

const LLM_KEYS: [&str; 10] = [
    "model",
    "reasoning_effort",
    "temperature",
    "top_p",
    "max_tokens",
    "mcp",
    "max_tool_rounds",
    "skills",
    "subagents",
    "tools",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocument {
    #[serde(rename = "version")]
    _version: Option<u32>,
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    inputs: serde_yaml::Mapping,
    input_schema: Option<serde_yaml::Value>,
    output: Option<String>,
    timeout: Option<u64>,
    #[serde(default)]
    default: WorkflowDefaults,
    #[serde(default)]
    models: ModelMap,
    #[serde(default)]
    schemas: BTreeMap<String, SchemaSource>,
    #[serde(default)]
    agents: BTreeMap<String, AgentFile>,
    steps: Vec<serde_yaml::Value>,
}

/// Parses `contents` as a workflow file located in `base_dir`.
pub(crate) fn parse_workflow(contents: &str, base_dir: &Path) -> Result<WorkflowFile> {
    let value: serde_yaml::Value = serde_yaml::from_str(contents)?;
    reject_other_versions(&value)?;
    let raw: RawDocument = serde_yaml::from_value(value)?;

    let mut schemas: SchemaMap = raw.schemas;
    for (name, source) in &mut schemas {
        check_name(name, "schema")?;
        source.resolve_relative_to(base_dir);
    }

    let mut agents = BTreeMap::new();
    for (name, mut definition) in raw.agents {
        check_name(&name, "agent")?;
        definition
            .finish_inline(base_dir)
            .with_context(|| format!("agents.{name}"))?;
        check_template(&definition.system_prompt_template)
            .with_context(|| format!("agents.{name}.system"))?;
        agents.insert(name, Arc::new(definition));
    }

    check_defaults(&raw.default)?;
    if let Some(timeout) = raw.timeout {
        check_timeout(timeout).context("'timeout'")?;
    }
    if let Some(output) = &raw.output {
        check_jq(output).context("'output'")?;
    }
    let inputs = parse_inputs(raw.inputs)?;

    let parser = Parser {
        schemas: &schemas,
        agents: &agents,
        base_dir,
    };
    let input_schema = raw
        .input_schema
        .map(|value| parser.schema_ref(value, "input_schema", "the workflow"))
        .transpose()?;
    let steps = parser.parse_steps(raw.steps, "steps")?;

    let mut ids = HashSet::new();
    check_ids(&steps, &mut ids)?;
    check_placement(&steps, Placement::default())?;

    Ok(WorkflowFile {
        name: raw.name,
        description: raw.description,
        inputs,
        input_schema,
        output: raw.output,
        timeout: raw.timeout,
        defaults: raw.default,
        models: raw.models,
        schemas,
        agents,
        steps,
    })
}

/// Rejects a pre-version-2 file with a pointer to the migration guide rather
/// than a string of "unknown field" errors, and any version this build
/// doesn't know.
fn reject_other_versions(value: &serde_yaml::Value) -> Result<()> {
    let Some(mapping) = value.as_mapping() else {
        bail!("a workflow file must be a YAML mapping");
    };
    let version = mapping.get("version");
    let legacy =
        mapping.contains_key("nodes") || version.and_then(serde_yaml::Value::as_u64) == Some(1);
    if legacy {
        bail!(
            "this is a version 1 workflow ('nodes:'/'use:'), which this build of lait no longer \
             runs; rewrite it for version 2 (see the migration guide in docs/usage/ja/workflow.md)"
        );
    }
    if let Some(version) = version
        && version.as_u64() != Some(u64::from(CURRENT_WORKFLOW_VERSION))
    {
        bail!(
            "unsupported workflow 'version: {}'; this build of lait supports version \
             {CURRENT_WORKFLOW_VERSION} (omit 'version:' to use it)",
            serde_yaml::to_string(version)
                .unwrap_or_default()
                .trim_end()
        );
    }
    Ok(())
}

fn parse_inputs(raw: serde_yaml::Mapping) -> Result<Vec<(String, InputDefinition)>> {
    let mut inputs = Vec::with_capacity(raw.len());
    for (key, value) in raw {
        let name = key
            .as_str()
            .ok_or_else(|| anyhow!("'inputs' keys must be strings"))?
            .to_owned();
        check_name(&name, "input")?;
        let schema = match value {
            serde_yaml::Value::String(type_name) => serde_json::json!({ "type": type_name }),
            serde_yaml::Value::Mapping(_) => serde_json::to_value(&value)
                .with_context(|| format!("inputs.{name}: invalid schema"))?,
            serde_yaml::Value::Null => serde_json::json!({}),
            _ => bail!(
                "inputs.{name}: expected a JSON Schema mapping or a type name such as 'string'"
            ),
        };
        let default = schema.get("default").cloned();
        let description = schema
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let Some(default) = &default {
            schema::validate_value(&schema, default, &format!("inputs.{name}.default"))?;
        }
        inputs.push((
            name,
            InputDefinition {
                schema,
                default,
                description,
            },
        ));
    }
    Ok(inputs)
}

struct Parser<'a> {
    schemas: &'a SchemaMap,
    agents: &'a BTreeMap<String, Arc<AgentFile>>,
    base_dir: &'a Path,
}

/// A step's remaining YAML fields while its kind-specific parser takes the
/// ones it knows; anything left over is an unknown field for that kind.
struct Fields {
    map: serde_yaml::Mapping,
    at: String,
}

impl Fields {
    fn take<T: DeserializeOwned>(&mut self, key: &str) -> Result<Option<T>> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(value) => serde_yaml::from_value(value)
                .map(Some)
                .with_context(|| format!("{}: invalid '{key}'", self.at)),
        }
    }

    fn take_value(&mut self, key: &str) -> Option<serde_yaml::Value> {
        self.map.remove(key)
    }

    fn require<T: DeserializeOwned>(&mut self, key: &str, kind: &str) -> Result<T> {
        self.take(key)?
            .ok_or_else(|| anyhow!("{}: a '{kind}' step requires '{key}'", self.at))
    }

    /// Rejects any field no parser took. `what` names the kind of mapping
    /// in the message (e.g. `'jq' steps`).
    fn finish(self, what: &str, allowed: &[&str]) -> Result<()> {
        if let Some((key, _)) = self.map.into_iter().next() {
            let key = serde_yaml::to_string(&key).unwrap_or_default();
            bail!(
                "{}: unknown field '{}'; {what} accept: {}",
                self.at,
                key.trim_end(),
                allowed.join(", ")
            );
        }
        Ok(())
    }
}

impl Parser<'_> {
    fn parse_steps(&self, raw: Vec<serde_yaml::Value>, at: &str) -> Result<Vec<Step>> {
        if raw.is_empty() {
            bail!("{at}: must contain at least one step");
        }
        raw.into_iter()
            .enumerate()
            .map(|(index, value)| self.parse_step(value, &format!("{at}[{index}]")))
            .collect()
    }

    fn parse_step_list(&self, value: serde_yaml::Value, at: &str) -> Result<Vec<Step>> {
        let raw: Vec<serde_yaml::Value> = serde_yaml::from_value(value)
            .with_context(|| format!("{at}: expected a list of steps"))?;
        self.parse_steps(raw, at)
    }

    fn parse_step(&self, value: serde_yaml::Value, at: &str) -> Result<Step> {
        let serde_yaml::Value::Mapping(map) = value else {
            bail!("{at}: a step must be a mapping");
        };
        let kinds: Vec<&str> = KIND_KEYS
            .into_iter()
            .filter(|key| map.contains_key(*key))
            .collect();
        let kind_key = match kinds.as_slice() {
            [kind] => *kind,
            [] => bail!(
                "{at}: a step needs exactly one of: {}{}",
                KIND_KEYS.join(", "),
                if map.contains_key("use") {
                    " ('use:' is version 1 syntax; see the migration guide)"
                } else {
                    ""
                }
            ),
            several => bail!(
                "{at}: a step must have exactly one kind, found {}",
                several.join(" and ")
            ),
        };
        let id = map.get("id").and_then(serde_yaml::Value::as_str);
        let at = match id {
            Some(id) => format!("{at} (id '{id}')"),
            None => at.to_owned(),
        };
        let mut fields = Fields {
            map,
            at: at.clone(),
        };

        let id: Option<String> = fields.take("id")?;
        if let Some(id) = &id {
            check_name(id, "step id").with_context(|| at.clone())?;
        }
        let when: Option<String> = fields.take("when")?;
        let input: Option<String> = fields.take("input")?;
        let output: Option<String> = fields.take("output")?;
        for (key, filter) in [("when", &when), ("input", &input), ("output", &output)] {
            if let Some(filter) = filter {
                check_jq(filter).with_context(|| format!("{at}: '{key}'"))?;
            }
        }
        let control = matches!(kind_key, "stop" | "break");
        let (retry, timeout, on_error) = if control {
            (None, None, None)
        } else {
            let retry: Option<RetryPolicy> = fields.take("retry")?;
            if let Some(retry) = &retry {
                check_retry(retry).with_context(|| format!("{at}: 'retry'"))?;
            }
            let timeout: Option<u64> = fields.take("timeout")?;
            if let Some(timeout) = timeout {
                check_timeout(timeout).with_context(|| format!("{at}: 'timeout'"))?;
            }
            let on_error = fields
                .take_value("on_error")
                .map(|value| self.parse_step_list(value, &format!("{at}.on_error")))
                .transpose()?;
            (retry, timeout, on_error)
        };

        let kind = self.parse_kind(kind_key, &mut fields, &at)?;
        let mut allowed: Vec<&str> = vec![kind_key];
        allowed.extend(if control {
            &COMMON_KEYS[..4]
        } else {
            &COMMON_KEYS[..]
        });
        allowed.extend(kind_fields(kind_key));
        fields.finish(&format!("'{kind_key}' steps"), &allowed)?;

        Ok(Step {
            id,
            when,
            input,
            output,
            retry,
            timeout,
            on_error,
            kind,
        })
    }

    fn parse_kind(&self, kind_key: &str, fields: &mut Fields, at: &str) -> Result<StepKind> {
        Ok(match kind_key {
            "prompt" => {
                let prompt: String = fields.require("prompt", "prompt")?;
                check_template(&prompt).with_context(|| format!("{at}: 'prompt'"))?;
                let system: Option<String> = fields.take("system")?;
                if let Some(system) = &system {
                    check_template(system).with_context(|| format!("{at}: 'system'"))?;
                }
                let llm = take_llm(fields, at)?;
                let attachments = take_attachments(fields, at)?;
                let input_schema = self.take_schema(fields, "input_schema", at)?;
                let output_schema = self.take_schema(fields, "output_schema", at)?;
                let schema_name: Option<String> = fields.take("schema_name")?;
                if let Some(name) = &schema_name {
                    if output_schema.is_none() {
                        bail!("{at}: 'schema_name' requires 'output_schema'");
                    }
                    schema::validate_schema_name(name).with_context(|| at.to_owned())?;
                }
                let step = PromptStep {
                    prompt,
                    system,
                    llm,
                    attachments,
                    input_schema,
                    output_schema,
                    schema_name,
                };
                if step.output_schema.is_some() {
                    schema::validate_schema_name(step.effective_schema_name()).with_context(
                        || format!("{at}: the schema name (set 'schema_name' to override it)"),
                    )?;
                }
                StepKind::Prompt(Box::new(step))
            }
            "agent" => {
                let reference: String = fields.require("agent", "agent")?;
                let agent = self.resolve_agent(&reference, at)?;
                let llm = take_llm(fields, at)?;
                let attachments = take_attachments(fields, at)?;
                StepKind::Agent(Box::new(AgentStep {
                    agent,
                    llm,
                    attachments,
                }))
            }
            "run" => {
                let argv: Vec<String> = fields.require("run", "run")?;
                if argv.first().is_none_or(|program| program.trim().is_empty()) {
                    bail!("{at}: 'run' must start with the program to execute");
                }
                for arg in &argv {
                    check_template(arg).with_context(|| format!("{at}: 'run'"))?;
                }
                StepKind::Run(RunStep { argv })
            }
            "workflow" => {
                let reference: String = fields.require("workflow", "workflow")?;
                let workflow = if looks_like_path(&reference, &["yml", "yaml"]) {
                    WorkflowRef::Path(self.base_dir.join(&reference))
                } else {
                    WorkflowRef::Registry(reference)
                };
                let with: Option<String> = fields.take("with")?;
                if let Some(with) = &with {
                    check_jq(with).with_context(|| format!("{at}: 'with'"))?;
                }
                StepKind::Workflow(WorkflowStep { workflow, with })
            }
            "jq" => {
                let filter: String = fields.require("jq", "jq")?;
                check_jq(&filter).with_context(|| format!("{at}: 'jq'"))?;
                StepKind::Jq(filter)
            }
            "ask" => {
                let prompt: String = fields.require("ask", "ask")?;
                if prompt.trim().is_empty() {
                    bail!("{at}: 'ask' needs a non-empty question");
                }
                check_template(&prompt).with_context(|| format!("{at}: 'ask'"))?;
                let choices: Option<Vec<String>> = fields.take("choices")?;
                let default: Option<String> = fields.take("default")?;
                let multiline: Option<bool> = fields.take("multiline")?;
                if let Some(choices) = &choices {
                    if choices.is_empty() || choices.iter().any(String::is_empty) {
                        bail!("{at}: 'choices' must be a non-empty list of non-empty strings");
                    }
                    if let Some(default) = &default
                        && !choices.contains(default)
                    {
                        bail!("{at}: 'default' ({default:?}) must be one of 'choices'");
                    }
                }
                StepKind::Ask(AskStep {
                    prompt,
                    choices,
                    default,
                    multiline: multiline.unwrap_or(false),
                })
            }
            "write" => {
                let path: String = fields.require("write", "write")?;
                if path.trim().is_empty() {
                    bail!("{at}: 'write' needs a path");
                }
                check_template(&path).with_context(|| format!("{at}: 'write'"))?;
                StepKind::Write(WriteStep { path })
            }
            "group" => {
                let value = fields.take_value("group").unwrap_or_default();
                StepKind::Group(self.parse_step_list(value, &format!("{at}.group"))?)
            }
            "switch" => {
                let value = fields.take_value("switch").unwrap_or_default();
                let raw_cases: Vec<serde_yaml::Mapping> = serde_yaml::from_value(value)
                    .with_context(|| format!("{at}: 'switch' must be a list of cases"))?;
                if raw_cases.is_empty() {
                    bail!("{at}: 'switch' must contain at least one case");
                }
                let mut cases = Vec::with_capacity(raw_cases.len());
                for (index, case) in raw_cases.into_iter().enumerate() {
                    let case_at = format!("{at}.switch[{index}]");
                    let mut case_fields = Fields {
                        map: case,
                        at: case_at.clone(),
                    };
                    let when: String = case_fields.require("when", "switch case")?;
                    check_jq(&when).with_context(|| format!("{case_at}: 'when'"))?;
                    let steps_value = case_fields
                        .take_value("steps")
                        .ok_or_else(|| anyhow!("{case_at}: a case requires 'steps'"))?;
                    let steps = self.parse_step_list(steps_value, &format!("{case_at}.steps"))?;
                    case_fields.finish("switch cases", &["when", "steps"])?;
                    cases.push(Case { when, steps });
                }
                let else_steps = fields
                    .take_value("else")
                    .map(|value| self.parse_step_list(value, &format!("{at}.else")))
                    .transpose()?;
                StepKind::Switch(SwitchStep { cases, else_steps })
            }
            "parallel" => {
                let value = fields.take_value("parallel").unwrap_or_default();
                let serde_yaml::Value::Mapping(raw_branches) = value else {
                    bail!("{at}: 'parallel' must map branch names to step lists");
                };
                if raw_branches.is_empty() {
                    bail!("{at}: 'parallel' must contain at least one branch");
                }
                let mut branches = Vec::with_capacity(raw_branches.len());
                for (key, steps) in raw_branches {
                    let name = key
                        .as_str()
                        .ok_or_else(|| anyhow!("{at}: 'parallel' branch names must be strings"))?
                        .to_owned();
                    check_name(&name, "branch name").with_context(|| at.to_owned())?;
                    let steps = self.parse_step_list(steps, &format!("{at}.parallel.{name}"))?;
                    branches.push((name, steps));
                }
                StepKind::Parallel(ParallelStep { branches })
            }
            "for_each" => {
                let items: String = fields.require("for_each", "for_each")?;
                check_jq(&items).with_context(|| format!("{at}: 'for_each'"))?;
                let steps = self.take_body(fields, "for_each", at)?;
                let max_concurrency: Option<usize> = fields.take("max_concurrency")?;
                if max_concurrency == Some(0) {
                    bail!("{at}: 'max_concurrency' must be at least 1");
                }
                StepKind::ForEach(ForEachStep {
                    items,
                    steps,
                    max_concurrency: max_concurrency.unwrap_or(1),
                })
            }
            "while" | "until" => {
                let condition: String = fields.require(kind_key, kind_key)?;
                check_jq(&condition).with_context(|| format!("{at}: '{kind_key}'"))?;
                let steps = self.take_body(fields, kind_key, at)?;
                let max_iterations: usize = fields.require("max_iterations", kind_key)?;
                if max_iterations == 0 {
                    bail!("{at}: 'max_iterations' must be at least 1");
                }
                StepKind::Loop(LoopStep {
                    condition: if kind_key == "while" {
                        LoopCondition::While(condition)
                    } else {
                        LoopCondition::Until(condition)
                    },
                    max_iterations,
                    steps,
                })
            }
            "stop" | "break" => {
                let flag: bool = fields.require(kind_key, kind_key)?;
                if !flag {
                    bail!("{at}: '{kind_key}' only accepts 'true'");
                }
                if kind_key == "stop" {
                    StepKind::Stop
                } else {
                    StepKind::Break
                }
            }
            other => bail!("{at}: internal error: unhandled step kind '{other}'"),
        })
    }

    fn take_body(&self, fields: &mut Fields, kind: &str, at: &str) -> Result<Vec<Step>> {
        let value = fields
            .take_value("steps")
            .ok_or_else(|| anyhow!("{at}: a '{kind}' step requires 'steps'"))?;
        self.parse_step_list(value, &format!("{at}.steps"))
    }

    fn take_schema(&self, fields: &mut Fields, key: &str, at: &str) -> Result<Option<SchemaRef>> {
        fields
            .take_value(key)
            .map(|value| self.schema_ref(value, key, at))
            .transpose()
    }

    /// Resolves a schema reference: a `schemas:` name, an inline schema, or
    /// `{file: ...}` relative to the workflow file.
    fn schema_ref(&self, value: serde_yaml::Value, key: &str, at: &str) -> Result<SchemaRef> {
        if let serde_yaml::Value::String(name) = &value {
            let source = self.schemas.get(name).ok_or_else(|| {
                anyhow!(
                    "{at}: '{key}' refers to schema '{name}', which is not defined in 'schemas:'"
                )
            })?;
            return Ok(SchemaRef {
                name: Some(name.clone()),
                source: source.clone(),
            });
        }
        let mut source: SchemaSource =
            serde_yaml::from_value(value).with_context(|| format!("{at}: invalid '{key}'"))?;
        source.resolve_relative_to(self.base_dir);
        Ok(SchemaRef { name: None, source })
    }

    fn resolve_agent(&self, reference: &str, at: &str) -> Result<AgentRef> {
        if let Some(definition) = self.agents.get(reference) {
            return Ok(AgentRef::Inline {
                name: reference.to_owned(),
                definition: Arc::clone(definition),
            });
        }
        if looks_like_path(reference, &["md"]) {
            return Ok(AgentRef::Path(self.base_dir.join(reference)));
        }
        if reference.is_empty() {
            bail!("{at}: 'agent' must not be empty");
        }
        Ok(AgentRef::Registry(reference.to_owned()))
    }
}

/// The kind-specific fields each kind key accepts, besides the common ones.
pub(crate) fn kind_fields(kind_key: &str) -> Vec<&'static str> {
    let mut fields: Vec<&'static str> = match kind_key {
        "prompt" => vec![
            "system",
            "files",
            "images",
            "input_schema",
            "output_schema",
            "schema_name",
        ],
        "agent" => vec!["files", "images"],
        "workflow" => vec!["with"],
        "ask" => vec!["choices", "default", "multiline"],
        "switch" => vec!["else"],
        "for_each" => vec!["steps", "max_concurrency"],
        "while" | "until" => vec!["steps", "max_iterations"],
        _ => Vec::new(),
    };
    if matches!(kind_key, "prompt" | "agent") {
        fields.extend(LLM_KEYS);
    }
    fields
}

fn take_llm(fields: &mut Fields, at: &str) -> Result<LlmOverrides> {
    let mut map = serde_yaml::Mapping::new();
    for key in LLM_KEYS {
        if let Some(value) = fields.take_value(key) {
            map.insert(serde_yaml::Value::String(key.to_owned()), value);
        }
    }
    let overrides: LlmOverrides = serde_yaml::from_value(serde_yaml::Value::Mapping(map))
        .with_context(|| format!("{at}: invalid model settings"))?;
    llm::validate_sampling_params(
        overrides.temperature,
        overrides.top_p,
        overrides.max_tokens,
        at,
    )?;
    llm::validate_max_tool_rounds(overrides.max_tool_rounds, at)?;
    Ok(overrides)
}

fn take_attachments(fields: &mut Fields, at: &str) -> Result<Attachments> {
    let files: Vec<String> = fields.take("files")?.unwrap_or_default();
    let images: Vec<String> = fields.take("images")?.unwrap_or_default();
    for template_source in files.iter().chain(&images) {
        check_template(template_source).with_context(|| format!("{at}: attachment"))?;
    }
    Ok(Attachments { files, images })
}

fn check_defaults(defaults: &WorkflowDefaults) -> Result<()> {
    llm::validate_sampling_params(
        defaults.temperature,
        defaults.top_p,
        defaults.max_tokens,
        "the workflow's 'default'",
    )?;
    llm::validate_max_tool_rounds(defaults.max_tool_rounds, "the workflow's 'default'")?;
    if let Some(retry) = &defaults.retry {
        check_retry(retry).context("'default.retry'")?;
    }
    if let Some(timeout) = defaults.timeout {
        check_timeout(timeout).context("'default.timeout'")?;
    }
    if let Some(system) = &defaults.system {
        check_template(system).context("'default.system'")?;
    }
    Ok(())
}

fn check_retry(retry: &RetryPolicy) -> Result<()> {
    if retry.max_attempts == 0 {
        bail!("'max_attempts' must be at least 1");
    }
    if !retry.backoff.is_finite() || retry.backoff < 0.0 {
        bail!("'backoff' must be a finite, non-negative number");
    }
    Ok(())
}

fn check_timeout(timeout: u64) -> Result<()> {
    if timeout == 0 {
        bail!("must be at least 1 second");
    }
    Ok(())
}

fn check_jq(filter: &str) -> Result<()> {
    jq::check_syntax(filter)
}

fn check_template(source: &str) -> Result<()> {
    template::check_syntax(source)
}

/// Ids, input names, schema/agent names, and branch names share one shape so
/// they work as `{{ steps.<id> }}` template paths and `$steps.<id>` jq paths
/// alike: a letter or underscore, then letters, digits, `_`, or `-`.
fn check_name(name: &str, what: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        bail!(
            "invalid {what} {name:?}: use a letter or '_' followed by letters, digits, '_' or '-'"
        );
    }
    Ok(())
}

fn check_ids(steps: &[Step], seen: &mut HashSet<String>) -> Result<()> {
    for step in steps {
        if let Some(id) = &step.id
            && !seen.insert(id.clone())
        {
            bail!(
                "step id '{id}' is used more than once; ids must be unique within a workflow file"
            );
        }
        let mut result = Ok(());
        step.for_each_child(|_, children| {
            if result.is_ok() {
                result = check_ids(children, seen);
            }
        });
        result?;
    }
    Ok(())
}

/// Where a step list sits relative to loops and concurrency, for the
/// `break`/`stop`/`ask`/`write` placement rules.
#[derive(Clone, Copy, Default)]
struct Placement {
    /// Inside a loop body reachable without crossing a concurrent boundary.
    in_loop: bool,
    /// Inside a `parallel` branch or a concurrent `for_each` body.
    concurrent: bool,
    /// Inside a concurrent `for_each` body specifically.
    concurrent_items: bool,
}

fn check_placement(steps: &[Step], placement: Placement) -> Result<()> {
    for step in steps {
        let label = step.id.as_deref().unwrap_or(step.kind.name());
        match &step.kind {
            StepKind::Break if !placement.in_loop => bail!(
                "'break' ({label}) must be inside a 'for_each'/'while'/'until' body \
                 (and cannot cross a 'parallel' branch or a concurrent 'for_each')"
            ),
            StepKind::Stop if placement.concurrent => bail!(
                "'stop' ({label}) cannot be used inside a 'parallel' branch or a concurrent \
                 'for_each' body"
            ),
            StepKind::Ask(_) if placement.concurrent => bail!(
                "'ask' ({label}) cannot run inside a 'parallel' branch or a concurrent \
                 'for_each' body, where several prompts would compete for stdin"
            ),
            StepKind::Write(write) if placement.concurrent_items && !write.is_dynamic() => bail!(
                "'write' ({label}) with a fixed path cannot run inside a concurrent 'for_each'; \
                 include a placeholder such as '{{{{ loop.index }}}}' in the path or move the \
                 write after the loop"
            ),
            _ => {}
        }
        let mut result = Ok(());
        step.for_each_child(|child, children| {
            if result.is_err() {
                return;
            }
            let nested = match child {
                Child::OnError | Child::Group | Child::Case => placement,
                Child::LoopBody => Placement {
                    in_loop: true,
                    ..placement
                },
                Child::Branch => Placement {
                    in_loop: false,
                    concurrent: true,
                    ..placement
                },
                Child::ConcurrentItem => Placement {
                    in_loop: false,
                    concurrent: true,
                    concurrent_items: true,
                },
            };
            result = check_placement(children, nested);
        });
        result?;
    }
    Ok(())
}
