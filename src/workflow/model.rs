//! The validated, executable shape of a workflow file (schema version 2).
//! Built only by `super::parse`, which checks every static rule before any
//! of these values exist; the interpreter, dry-run, graph, and linter only
//! ever see this shape.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::Deserialize;

use crate::{agent::AgentFile, config::ModelMap, reasoning::ReasoningEffort, schema::SchemaSource};

/// The workflow schema version this build understands. `version:` may be
/// omitted (meaning this version); any other explicit value is rejected.
pub(crate) const CURRENT_WORKFLOW_VERSION: u32 = 2;

/// A parsed and validated workflow file.
#[derive(Debug)]
pub(crate) struct WorkflowFile {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    /// Declared named parameters, in declaration order: bound from
    /// `lait run --input KEY=VALUE`, a `workflow:` step's `with:`, or a test
    /// definition's `inputs:`, and exposed as `{{ inputs.<name> }}`/
    /// `$inputs.<name>`.
    pub(crate) inputs: Vec<(String, InputDefinition)>,
    /// The schema of the initial value (PROMPT, a caller's step input, or a
    /// test definition's `input`). Also decides how PROMPT text is read: kept
    /// as a string for `type: string` (or no schema), parsed as JSON
    /// otherwise.
    pub(crate) input_schema: Option<SchemaRef>,
    /// A jq expression computing the workflow's result from the last step's
    /// value (`.`), `$steps`, and `$inputs`. Absent means the last value.
    pub(crate) output: Option<String>,
    /// A wall-clock limit (seconds) on one run of this file, including when
    /// it runs as another workflow's child.
    pub(crate) timeout: Option<u64>,
    pub(crate) defaults: WorkflowDefaults,
    pub(crate) models: ModelMap,
    pub(crate) schemas: crate::schema::SchemaMap,
    pub(crate) agents: BTreeMap<String, Arc<AgentFile>>,
    pub(crate) steps: Vec<Step>,
}

impl WorkflowFile {
    /// Whether this workflow declares any `inputs:`. A workflow that does
    /// may be run without a PROMPT.
    pub(crate) fn declares_inputs(&self) -> bool {
        !self.inputs.is_empty()
    }
}

/// One `inputs:` entry: a JSON Schema describing the value. An entry with a
/// `default` is optional; every other entry is required.
#[derive(Debug, Clone)]
pub(crate) struct InputDefinition {
    pub(crate) schema: serde_json::Value,
    pub(crate) default: Option<serde_json::Value>,
    pub(crate) description: Option<String>,
}

impl InputDefinition {
    /// Whether a raw `--input KEY=VALUE` string should be kept verbatim
    /// rather than parsed as JSON: true when the schema's `type` is exactly
    /// `string`, so `--input id=0012` stays the string `"0012"`.
    pub(crate) fn wants_raw_string(&self) -> bool {
        self.schema.get("type").and_then(serde_json::Value::as_str) == Some("string")
    }
}

/// A workflow's `default:` block: fallback settings for LLM steps
/// (`prompt`/`agent`) that don't set their own. A child workflow's own
/// entries take precedence over its caller's, field by field (see
/// [`WorkflowDefaults::fold`]).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowDefaults {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
    /// Default system prompt template for `prompt` steps without `system:`.
    pub(crate) system: Option<String>,
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Option<Vec<String>>,
    pub(crate) subagents: Option<Vec<String>>,
    pub(crate) tools: Option<Vec<String>>,
    /// Falls back as a whole policy, not field by field.
    pub(crate) retry: Option<RetryPolicy>,
    /// Per-attempt timeout (seconds).
    pub(crate) timeout: Option<u64>,
}

impl WorkflowDefaults {
    /// Merges layers, priority-ordered (`layers[0]` wins): each field
    /// independently takes the first layer that sets it.
    pub(crate) fn fold(layers: &[Self]) -> Self {
        Self {
            model: layers.iter().find_map(|layer| layer.model.clone()),
            reasoning_effort: layers.iter().find_map(|layer| layer.reasoning_effort),
            temperature: layers.iter().find_map(|layer| layer.temperature),
            top_p: layers.iter().find_map(|layer| layer.top_p),
            max_tokens: layers.iter().find_map(|layer| layer.max_tokens),
            system: layers.iter().find_map(|layer| layer.system.clone()),
            mcp: layers.iter().find_map(|layer| layer.mcp.clone()),
            max_tool_rounds: layers.iter().find_map(|layer| layer.max_tool_rounds),
            skills: layers.iter().find_map(|layer| layer.skills.clone()),
            subagents: layers.iter().find_map(|layer| layer.subagents.clone()),
            tools: layers.iter().find_map(|layer| layer.tools.clone()),
            retry: layers.iter().find_map(|layer| layer.retry.clone()),
            timeout: layers.iter().find_map(|layer| layer.timeout),
        }
    }
}

/// `retry:` — total attempts (including the first), the wait before the
/// first retry, and the multiplier applied to the wait after each retry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetryPolicy {
    pub(crate) max_attempts: usize,
    #[serde(default)]
    pub(crate) delay_seconds: u64,
    #[serde(default = "default_backoff")]
    pub(crate) backoff: f64,
}

fn default_backoff() -> f64 {
    1.0
}

/// One entry of a `steps:` list. Every step has exactly one kind (see
/// [`StepKind`]) plus the common fields below. Execution order for one step:
/// `when` → (`input` → the kind's action → `output`, retried/timed out as
/// one unit) → `on_error` on failure → record under `id`.
#[derive(Debug)]
pub(crate) struct Step {
    /// Records this step's final value as `steps.<id>`/`$steps.<id>`;
    /// unique within the file.
    pub(crate) id: Option<String>,
    /// A jq condition on the incoming value; falsy skips the step, passing
    /// the value through unchanged.
    pub(crate) when: Option<String>,
    /// A jq expression computing the value the kind acts on (default `.`).
    pub(crate) input: Option<String>,
    /// A jq expression mapping the kind's result (`.`) to this step's value.
    pub(crate) output: Option<String>,
    pub(crate) retry: Option<RetryPolicy>,
    pub(crate) timeout: Option<u64>,
    /// Runs with `{error, input}` in place of failing; its result becomes
    /// this step's value.
    pub(crate) on_error: Option<Vec<Step>>,
    pub(crate) kind: StepKind,
}

impl Step {
    /// This step's stable label: its `id`, else `step-<position>`.
    pub(crate) fn label_or(&self, position: usize) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("step-{position}"))
    }

    /// A progress label: the `id` when set, else `step-<n> (<kind>)`.
    pub(crate) fn progress_label(&self, counter: usize) -> String {
        match &self.id {
            Some(id) => id.clone(),
            None => format!("step-{counter} ({})", self.kind.name()),
        }
    }

    /// Whether this step calls a model, and therefore inherits
    /// `default.retry`/`default.timeout`.
    pub(crate) fn calls_model(&self) -> bool {
        matches!(self.kind, StepKind::Prompt(_) | StepKind::Agent(_))
    }

    /// Visits every nested step list directly under this step (not
    /// recursively), with how it is entered.
    pub(crate) fn for_each_child<'a>(&'a self, mut visit: impl FnMut(Child, &'a [Step])) {
        if let Some(on_error) = &self.on_error {
            visit(Child::OnError, on_error);
        }
        match &self.kind {
            StepKind::Group(steps) => visit(Child::Group, steps),
            StepKind::Switch(switch) => {
                for case in &switch.cases {
                    visit(Child::Case, &case.steps);
                }
                if let Some(else_steps) = &switch.else_steps {
                    visit(Child::Case, else_steps);
                }
            }
            StepKind::Parallel(parallel) => {
                for (_, steps) in &parallel.branches {
                    visit(Child::Branch, steps);
                }
            }
            StepKind::ForEach(for_each) => visit(
                if for_each.max_concurrency > 1 {
                    Child::ConcurrentItem
                } else {
                    Child::LoopBody
                },
                &for_each.steps,
            ),
            StepKind::Loop(loop_step) => visit(Child::LoopBody, &loop_step.steps),
            _ => {}
        }
    }
}

/// How a nested step list is entered, which decides whether `break`/`stop`
/// can cross into it and whether it runs concurrently with siblings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Child {
    OnError,
    Group,
    Case,
    Branch,
    LoopBody,
    ConcurrentItem,
}

#[derive(Debug)]
pub(crate) enum StepKind {
    Prompt(Box<PromptStep>),
    Agent(Box<AgentStep>),
    Run(RunStep),
    Workflow(WorkflowStep),
    Jq(String),
    Ask(AskStep),
    Write(WriteStep),
    Group(Vec<Step>),
    Switch(SwitchStep),
    Parallel(ParallelStep),
    ForEach(ForEachStep),
    Loop(LoopStep),
    Stop,
    Break,
}

impl StepKind {
    /// The kind's key as written in YAML.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Prompt(_) => "prompt",
            Self::Agent(_) => "agent",
            Self::Run(_) => "run",
            Self::Workflow(_) => "workflow",
            Self::Jq(_) => "jq",
            Self::Ask(_) => "ask",
            Self::Write(_) => "write",
            Self::Group(_) => "group",
            Self::Switch(_) => "switch",
            Self::Parallel(_) => "parallel",
            Self::ForEach(_) => "for_each",
            Self::Loop(loop_step) => loop_step.condition.keyword(),
            Self::Stop => "stop",
            Self::Break => "break",
        }
    }
}

/// Model/sampling/capability overrides shared by `prompt` and `agent`
/// steps. Each falls back independently: step → (agent definition) →
/// workflow `default:` → `lait.config.yml` `default:`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LlmOverrides {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Option<Vec<String>>,
    pub(crate) subagents: Option<Vec<String>>,
    pub(crate) tools: Option<Vec<String>>,
}

/// `files:`/`images:` attachments of an LLM step: templates rendered per
/// run, resolved relative to the current working directory.
#[derive(Debug, Default, Clone)]
pub(crate) struct Attachments {
    pub(crate) files: Vec<String>,
    pub(crate) images: Vec<String>,
}

/// A resolved schema reference: the source plus the `schemas:` name it was
/// referenced by (used as the default Structured Outputs schema name).
#[derive(Debug, Clone)]
pub(crate) struct SchemaRef {
    pub(crate) name: Option<String>,
    pub(crate) source: SchemaSource,
}

impl SchemaRef {
    pub(crate) fn describe(&self) -> String {
        match &self.name {
            Some(name) => format!("'{name}'"),
            None => self.source.describe(),
        }
    }
}

/// `prompt:` — an ad-hoc model call.
#[derive(Debug)]
pub(crate) struct PromptStep {
    pub(crate) prompt: String,
    pub(crate) system: Option<String>,
    pub(crate) llm: LlmOverrides,
    pub(crate) attachments: Attachments,
    pub(crate) input_schema: Option<SchemaRef>,
    pub(crate) output_schema: Option<SchemaRef>,
    pub(crate) schema_name: Option<String>,
}

impl PromptStep {
    /// The Structured Outputs schema name: `schema_name`, else the
    /// `schemas:` name the output schema was referenced by, else
    /// `structured_output`.
    pub(crate) fn effective_schema_name(&self) -> &str {
        self.schema_name
            .as_deref()
            .or_else(|| {
                self.output_schema
                    .as_ref()
                    .and_then(|schema| schema.name.as_deref())
            })
            .unwrap_or("structured_output")
    }
}

/// `agent:` — a model call through an agent definition.
#[derive(Debug)]
pub(crate) struct AgentStep {
    pub(crate) agent: AgentRef,
    pub(crate) llm: LlmOverrides,
    pub(crate) attachments: Attachments,
}

#[derive(Debug, Clone)]
pub(crate) enum AgentRef {
    /// An entry of this workflow's own `agents:`.
    Inline {
        name: String,
        definition: Arc<AgentFile>,
    },
    /// An agent Markdown file, resolved against the workflow's directory.
    Path(PathBuf),
    /// An `agents:` entry of `lait.config.yml`, resolved at run time.
    Registry(String),
}

impl AgentRef {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Inline { name, .. } => format!("{name} (inline)"),
            Self::Path(path) => path.display().to_string(),
            Self::Registry(name) => format!("{name} (lait.config.yml agents)"),
        }
    }
}

/// `run:` — a command, executed directly (no shell).
#[derive(Debug)]
pub(crate) struct RunStep {
    pub(crate) argv: Vec<String>,
}

/// `workflow:` — another workflow file, run as a child.
#[derive(Debug)]
pub(crate) struct WorkflowStep {
    pub(crate) workflow: WorkflowRef,
    /// A jq expression producing the child's `inputs` object.
    pub(crate) with: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum WorkflowRef {
    Path(PathBuf),
    Registry(String),
}

impl WorkflowRef {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Path(path) => path.display().to_string(),
            Self::Registry(name) => format!("{name} (lait.config.yml workflows)"),
        }
    }
}

/// `ask:` — a human answer read from an interactive terminal.
#[derive(Debug)]
pub(crate) struct AskStep {
    pub(crate) prompt: String,
    pub(crate) choices: Option<Vec<String>>,
    pub(crate) default: Option<String>,
    pub(crate) multiline: bool,
}

/// `write:` — writes the incoming value's text form to a file.
#[derive(Debug)]
pub(crate) struct WriteStep {
    /// A path template, resolved relative to the current working directory.
    pub(crate) path: String,
}

impl WriteStep {
    /// Whether the path contains a template placeholder (and so may differ
    /// between concurrently running items).
    pub(crate) fn is_dynamic(&self) -> bool {
        self.path.contains("{{")
    }
}

#[derive(Debug)]
pub(crate) struct SwitchStep {
    pub(crate) cases: Vec<Case>,
    pub(crate) else_steps: Option<Vec<Step>>,
}

#[derive(Debug)]
pub(crate) struct Case {
    pub(crate) when: String,
    pub(crate) steps: Vec<Step>,
}

#[derive(Debug)]
pub(crate) struct ParallelStep {
    /// Branch name → steps, in declaration order (also the key order of the
    /// joined object).
    pub(crate) branches: Vec<(String, Vec<Step>)>,
}

#[derive(Debug)]
pub(crate) struct ForEachStep {
    /// A jq expression producing the array to iterate.
    pub(crate) items: String,
    pub(crate) steps: Vec<Step>,
    pub(crate) max_concurrency: usize,
}

#[derive(Debug)]
pub(crate) struct LoopStep {
    pub(crate) condition: LoopCondition,
    pub(crate) max_iterations: usize,
    pub(crate) steps: Vec<Step>,
}

#[derive(Debug)]
pub(crate) enum LoopCondition {
    /// Checked before every iteration against the current value.
    While(String),
    /// Checked after every iteration against its result.
    Until(String),
}

impl LoopCondition {
    pub(crate) fn keyword(&self) -> &'static str {
        match self {
            Self::While(_) => "while",
            Self::Until(_) => "until",
        }
    }

    pub(crate) fn filter(&self) -> &str {
        match self {
            Self::While(filter) | Self::Until(filter) => filter,
        }
    }
}

/// Whether `reference` names a file (contains a path separator or ends with
/// one of `extensions`) rather than a registry entry.
pub(crate) fn looks_like_path(reference: &str, extensions: &[&str]) -> bool {
    reference.contains('/')
        || reference.contains(std::path::MAIN_SEPARATOR)
        || extensions.iter().any(|extension| {
            Path::new(reference)
                .extension()
                .is_some_and(|actual| actual == *extension)
        })
}
