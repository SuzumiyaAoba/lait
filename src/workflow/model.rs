//! Validated workflow step/router/node types — the shapes `exec::run_steps`
//! actually interprets. These are reached only through `raw::FlowStep` +
//! `validate::validate_steps` (see `super`'s module doc for the
//! deserialize → validate → model pipeline); nothing outside that pipeline
//! constructs a `NodeDefinition` directly, which is what lets `exec` assume
//! every structural rule `validate` checks (router/action-field exclusivity,
//! duplicate labels, ...) already holds by the time it sees one.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::{config::ModelMap, reasoning::ReasoningEffort, schema::JsonSchemaMap};

/// The only workflow schema version this build understands. `WorkflowFile`'s
/// `version:` is optional (omitted means "latest"); an explicit but
/// unrecognized number is rejected outright rather than silently misparsed
/// — see `super::parse_workflow`.
pub(crate) const CURRENT_WORKFLOW_VERSION: u32 = 1;

/// The workflow-file-scoped map of reusable action definitions, keyed by the
/// name used in `steps[].use`. Unlike `models`/`json_schemas`, this is never
/// merged into a nested `workflow:` step's sub-workflow scope — each file's
/// `use:` resolves only against its own `nodes:` during validation. Compiled
/// call steps retain an Arc to that definition for their entire lifetime.
pub(crate) type NodeMap = BTreeMap<String, Arc<NodeDefinition>>;

pub(crate) type WorkflowFile = WorkflowDocument<FlowStep>;
pub(super) type RawWorkflowFile = WorkflowDocument<super::raw::FlowStep>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowDocument<S> {
    /// This file's schema version. `None` (the field omitted) means "the
    /// latest version this build supports" — the common case, and the only
    /// option before this field existed. An explicit version that isn't
    /// [`CURRENT_WORKFLOW_VERSION`] is rejected with a clear error instead
    /// of silently (mis)parsing against a schema the file wasn't written
    /// for, once a future schema change actually introduces a version 2.
    pub(crate) version: Option<u32>,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) default: WorkflowDefaults,
    /// Model aliases usable by `default.model`/`nodes[].model`, in the same shape as
    /// `lait.config.yml`'s top-level `models:`. Takes precedence over an alias of
    /// the same name defined in `lait.config.yml`.
    #[serde(default)]
    pub(crate) models: ModelMap,
    /// Named schema definitions usable by `nodes[].output_schema` and
    /// `nodes[].input_schema`, each either a `file_path:` to a JSON schema
    /// file or an inline `schema:` body.
    #[serde(default)]
    pub(crate) json_schemas: JsonSchemaMap,
    /// Reusable action definitions, referenced by `steps[].use`. A node
    /// describes *what* to do (a model call or data transform); it carries no
    /// information about *when* or *how many times* it runs — that lives on
    /// each `steps[].use` reference site instead, so the same node can be
    /// used from more than one place in `steps`.
    #[serde(default)]
    pub(crate) nodes: NodeMap,
    pub(crate) steps: Vec<S>,
}

/// A workflow file's `default:` block: the same `model`/`reasoning_effort`
/// fallback as `lait.config.yml`'s `default:` (see
/// `config::DefaultSettings`), plus a workflow-only `retry`/`timeout`
/// fallback applied to any step that calls a model (`prompt`/`agent`) and
/// doesn't set its own (see `NodeDefinition::settings`). Kept as its
/// own type rather than reusing `config::DefaultSettings` (`#[serde(flatten)]`
/// is documented as incompatible with `#[serde(deny_unknown_fields)]`, which
/// both this and `DefaultSettings` rely on to reject typos).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowDefaults {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Fallback sampling `temperature`/`top_p`/`max_tokens` for any step that
    /// calls a model and doesn't set its own. Unlike `retry`, each falls back
    /// independently (a step can override just one of the three).
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
    /// Fallback `retry` for any step that calls a model (`prompt`/`agent`)
    /// and doesn't set its own. Falls back as a whole struct, not
    /// field-by-field: a step with its own `retry: { max_attempts: 2 }` gets
    /// `delay_seconds: 0`/`backoff: 1.0` (the field's own defaults), not this
    /// `default.retry`'s `delay_seconds`/`backoff`.
    pub(crate) retry: Option<RetryDefinition>,
    /// Fallback `timeout` (seconds) for any step that calls a model
    /// (`prompt`/`agent`) and doesn't set its own.
    pub(crate) timeout: Option<u64>,
    /// Fallback `mcp`/`max_tool_rounds` for any node that calls a model
    /// (`prompt`/`agent`) and doesn't set its own. Each falls back
    /// independently, like `temperature`, not as a whole unit like `retry`.
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    /// Fallback `skills` for any node that calls a model (`prompt`/`agent`)
    /// and doesn't set its own. Falls back independently, like `mcp`.
    pub(crate) skills: Option<Vec<String>>,
    /// Fallback `subagents` for any node that calls a model (`prompt`/
    /// `agent`) and doesn't set its own. Falls back independently, like `mcp`.
    pub(crate) subagents: Option<Vec<String>>,
    /// Fallback `tools` for any node that calls a model (`prompt`/`agent`)
    /// and doesn't set its own. Falls back independently, like `mcp`.
    pub(crate) tools: Option<Vec<String>>,
    /// Fallback `system_prompt` for any `prompt` node that doesn't set its
    /// own. Falls back independently, like `mcp`. Meaningless for an `agent`
    /// node, which supplies its own system prompt from its agent file.
    pub(crate) system_prompt: Option<String>,
    /// A ceiling (seconds) on the *whole* run's wall-clock time, distinct
    /// from a node's own `timeout:`/`default.timeout` (which each bound a
    /// single step's action). Only enforced by `app::run_workflow` for the
    /// file passed directly to `lait run` — a `workflow:` node's own
    /// sub-workflow is instead bounded by that node's own `timeout:`, the
    /// same as any other node, so setting this inside a sub-workflow's
    /// `default:` has no effect of its own (it still folds like every other
    /// field here, in case a future caller wants to read it).
    pub(crate) workflow_timeout: Option<u64>,
}

impl WorkflowDefaults {
    /// Merges any number of layers, priority-ordered (`layers[0]` wins): each
    /// field independently takes the first layer that sets it. `retry` is one
    /// field here like any other — it falls back as a whole struct, never
    /// merged field-by-field (see its own doc above). Used by
    /// `WorkflowScope::nested` to merge a sub-workflow's `default:` over its
    /// caller's — the same `fold`-over-layers shape as
    /// `engine::{SamplingOverrides, CapabilityOverrides}::fold`.
    pub(crate) fn fold(layers: &[Self]) -> Self {
        Self {
            model: layers.iter().find_map(|layer| layer.model.clone()),
            reasoning_effort: layers.iter().find_map(|layer| layer.reasoning_effort),
            temperature: layers.iter().find_map(|layer| layer.temperature),
            top_p: layers.iter().find_map(|layer| layer.top_p),
            max_tokens: layers.iter().find_map(|layer| layer.max_tokens),
            retry: layers.iter().find_map(|layer| layer.retry.clone()),
            timeout: layers.iter().find_map(|layer| layer.timeout),
            mcp: layers.iter().find_map(|layer| layer.mcp.clone()),
            max_tool_rounds: layers.iter().find_map(|layer| layer.max_tool_rounds),
            skills: layers.iter().find_map(|layer| layer.skills.clone()),
            subagents: layers.iter().find_map(|layer| layer.subagents.clone()),
            tools: layers.iter().find_map(|layer| layer.tools.clone()),
            system_prompt: layers.iter().find_map(|layer| layer.system_prompt.clone()),
            workflow_timeout: layers.iter().find_map(|layer| layer.workflow_timeout),
        }
    }
}

/// A reusable action definition, referenced by id from `steps[].use`. Carries
/// only "what to do" — model call or data transform — never "when"/"how many
/// times", which lives on the `FlowStep` reference site instead.
///
/// Tagged by a required `type:` field (`prompt`/`agent`/`workflow`/`command`/
/// `transform`) rather than inferred from which fields are set: each variant
/// is its own struct with only the fields that make sense for it, so e.g.
/// `type: workflow` cannot also carry a `model:` — a typo/misunderstanding
/// that used to need one of `validate_node`'s ~15 hand-written mutual-
/// exclusion checks to catch is now simply a field the type doesn't have
/// (`#[serde(deny_unknown_fields)]` rejects it at parse time, before
/// `validate_node` ever runs). Fields shared by more than one variant (`jq`/
/// `write_file`/`retry`/`timeout`/sampling/capability knobs) are duplicated
/// per variant rather than `#[serde(flatten)]`ed out of a common struct:
/// `flatten` is documented as incompatible with `deny_unknown_fields` (see
/// `WorkflowDefaults`'s doc comment, which hit the same constraint first),
/// and losing the typo-rejection this DSL leans on for every field would
/// cost far more than the duplication does. `NodeDefinition::settings` below
/// exposes whichever shared fields a variant has as a borrowed view, with
/// `None` for fields that do not apply, so generic consumers outside the
/// variant-specific execution branches never have to match on the variant
/// themselves.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum NodeDefinition {
    Prompt(PromptNode),
    Agent(AgentNode),
    Workflow(WorkflowNode),
    Command(CommandNode),
    Transform(TransformNode),
    Ask(AskNode),
}

/// The action kind encoded by a node's `type:` field.
///
/// Keeping this classification separate from [`NodeDefinition`] lets callers
/// ask questions about a node's behavior without repeating a variant match.
/// The variants still carry the parsed fields; this enum only represents the
/// stable, field-independent part of the node contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Prompt,
    Agent,
    Workflow,
    Command,
    Transform,
    Ask,
}

impl NodeKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Agent => "agent",
            Self::Workflow => "workflow",
            Self::Command => "command",
            Self::Transform => "transform",
            Self::Ask => "ask",
        }
    }

    pub(crate) const fn calls_model(self) -> bool {
        matches!(self, Self::Prompt | Self::Agent)
    }

    pub(crate) const fn requires_interactive_stdin(self) -> bool {
        matches!(self, Self::Ask)
    }
}

/// Borrowed settings shared by node execution, validation, and presentation.
///
/// The YAML structs intentionally keep their fields variant-specific so
/// `deny_unknown_fields` can reject a setting on the wrong `type:`. This view
/// provides the opposite side of that boundary: consumers that only need
/// effective node metadata can read one uniform shape without repeating a
/// six-variant match for every field. Fields that do not apply to a variant
/// are represented as `None`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NodeSettings<'a> {
    pub(crate) model: Option<&'a str>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) mcp: Option<&'a [String]>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Option<&'a [String]>,
    pub(crate) subagents: Option<&'a [String]>,
    pub(crate) tools: Option<&'a [String]>,
    pub(crate) retry: Option<&'a RetryDefinition>,
    pub(crate) timeout: Option<u64>,
    pub(crate) jq: Option<&'a str>,
    pub(crate) write_file: Option<&'a Path>,
}

/// `type: prompt` — sends `prompt` (rendered as a handlebars template) and/or
/// `system_prompt` to the model. At least one of the two is required: a
/// `prompt` node that sends neither has nothing to distinguish it from
/// `type: transform`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptNode {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Sampling temperature (0.0-2.0) for this node's model call. Falls back
    /// independently to the workflow's `default.temperature` when unset (like
    /// `reasoning_effort`, not like `retry`'s whole-unit fallback).
    pub(crate) temperature: Option<f64>,
    /// Nucleus sampling probability mass (0.0-1.0) for this node's model
    /// call. Falls back independently to `default.top_p`, like `temperature`.
    pub(crate) top_p: Option<f64>,
    /// An upper bound on the number of tokens generated for this node's
    /// model call. Falls back independently to `default.max_tokens`, like
    /// `temperature`.
    pub(crate) max_tokens: Option<u32>,
    /// The user-message prompt template sent to the model. When unset but
    /// `system_prompt` is set, the node's current input is sent unchanged (no
    /// template rendering) as the user message instead.
    pub(crate) prompt: Option<String>,
    /// A system prompt template, rendered the same way as `prompt` (see
    /// `template::render`) and sent ahead of it as the system message. Falls
    /// back to the workflow's `default.system_prompt` when unset, the same
    /// way as `skills`.
    pub(crate) system_prompt: Option<String>,
    /// Paths whose contents are attached as context, like the CLI's
    /// `--file`: each is read as UTF-8 text and appended (as named fenced
    /// code blocks, see `attachment::read_file_attachments`) after the
    /// rendered `prompt` (or, for a `system_prompt`-only node, after the
    /// current input passed through unchanged).
    pub(crate) files: Option<Vec<PathBuf>>,
    /// Images attached for a vision-capable model, like the CLI's `--image`:
    /// each entry is a local file path (sent as a base64 data URL) or an
    /// `http(s)://` URL (sent as-is); see `attachment::resolve_image_urls`.
    pub(crate) images: Option<Vec<String>>,
    /// Validates this node's input before it runs (before rendering
    /// `prompt`). Resolved against the workflow's top-level `json_schemas:`
    /// first; if no such key exists, treated as a path to a JSON schema file
    /// instead.
    pub(crate) input_schema: Option<String>,
    /// Request a structured JSON response using the named schema, like the CLI's
    /// `--json-schema`. Resolved against the workflow's top-level `json_schemas:`
    /// first; if no such key exists, treated as a path to a JSON schema file
    /// instead.
    pub(crate) output_schema: Option<String>,
    /// The name of the structured output schema. Defaults to `structured_output`,
    /// like the CLI's `--schema-name`. Only used together with `output_schema`.
    pub(crate) schema_name: Option<String>,
    /// A jq filter applied to this node's output (the model's response)
    /// before it becomes `{{ input }}` for the next step. The filtered value
    /// must be valid JSON.
    pub(crate) jq: Option<String>,
    /// Writes this node's final output (after `jq`, if set) to this path,
    /// overwriting it if it already exists (parent directories are not
    /// created automatically). Resolved relative to the current working
    /// directory. Does not change what becomes `{{ input }}` for the next
    /// step. Rejected on a node used inside a `for_each` body whose
    /// `max_concurrency` is above 1, where every concurrently running item
    /// would write the same static path.
    pub(crate) write_file: Option<PathBuf>,
    /// Retries this node's action (`input_schema` check, model call, and
    /// `jq`, as one unit) up to `max_attempts` times on failure. Applies
    /// before the calling `FlowStep`'s `on_error`, which only runs once every
    /// attempt here has failed. Falls back to the workflow's `default.retry`
    /// (as a whole struct, not merged field-by-field) when unset.
    pub(crate) retry: Option<RetryDefinition>,
    /// A per-attempt time limit, in seconds, on this node's action. A timed
    /// out attempt counts as a failure for `retry`, the same as any other
    /// error. Falls back to the workflow's `default.timeout` under the same
    /// rule as `retry` above.
    pub(crate) timeout: Option<u64>,
    /// Names of `mcp_servers:` entries (from `lait.config.yml`) whose tools
    /// this node's model call may use. Falls back to the workflow's
    /// `default.mcp`, the same way as `reasoning_effort`.
    pub(crate) mcp: Option<Vec<String>>,
    /// The maximum number of tool-call round trips this node's model call may
    /// take before lait errors, when `mcp` (from any fallback layer) names at
    /// least one server. Falls back the same way as `mcp`.
    pub(crate) max_tool_rounds: Option<usize>,
    /// Names of `skills:` entries (from `lait.config.yml`) whose content is
    /// appended to this node's system prompt. Falls back to the workflow's
    /// `default.skills`, the same way as `mcp`.
    pub(crate) skills: Option<Vec<String>>,
    /// Names of `agents:` entries (from `lait.config.yml`) made available as
    /// callable subagent tools during this node's model call. Falls back to
    /// the workflow's `default.subagents`, the same way as `mcp`.
    pub(crate) subagents: Option<Vec<String>>,
    /// Names of `tools:` entries (from `lait.config.yml`) made available as
    /// callable shell-command tools during this node's model call. Falls
    /// back to the workflow's `default.tools`, the same way as `mcp`.
    pub(crate) tools: Option<Vec<String>>,
}

/// `type: agent` — runs an agent Markdown file (see `agent::load_agent`)
/// against this node's current input, the same way `lait agent run` does.
/// The agent file supplies its own system prompt and input/output schema, so
/// this variant has no `system_prompt`/`input_schema`/`output_schema`/
/// `schema_name` of its own — only `model`/sampling/capability overrides, all
/// applied on top of the agent file's own settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentNode {
    /// Path to an agent Markdown file, resolved relative to the current
    /// working directory (not relative to the workflow file, unlike
    /// `type: workflow`'s `workflow:`).
    pub(crate) agent: PathBuf,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
    /// Same as `PromptNode::files` — attached after the current input, which
    /// this node's agent call sends unchanged as its user message.
    pub(crate) files: Option<Vec<PathBuf>>,
    /// Same as `PromptNode::images`.
    pub(crate) images: Option<Vec<String>>,
    pub(crate) jq: Option<String>,
    pub(crate) write_file: Option<PathBuf>,
    pub(crate) retry: Option<RetryDefinition>,
    pub(crate) timeout: Option<u64>,
    /// Falls back to the agent file's own `mcp:`, then the workflow's
    /// `default.mcp`.
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    /// Falls back to the agent file's own `skills:`, then the workflow's
    /// `default.skills`.
    pub(crate) skills: Option<Vec<String>>,
    /// Falls back to the agent file's own `subagents:`, then the workflow's
    /// `default.subagents`.
    pub(crate) subagents: Option<Vec<String>>,
    /// Falls back to the agent file's own `tools:`, then the workflow's
    /// `default.tools`.
    pub(crate) tools: Option<Vec<String>>,
}

/// `type: workflow` — runs another workflow YAML file against this node's
/// input; that sub-workflow's final output becomes this node's output. Its
/// own `default:`/`models:`/`json_schemas:` take precedence, falling back to
/// this workflow's when it doesn't define an entry (see
/// `WorkflowScope::nested`). Every model-call/capability knob
/// (`model`/sampling/`mcp`/`skills`/`subagents`/`retry`/`timeout`/
/// `input_schema`/`output_schema`/`schema_name`/`system_prompt`/`files`/
/// `images`) lives on the sub-workflow's own steps instead — this variant
/// simply has none of those fields, so setting one is a parse-time "unknown
/// field" error rather than a `validate_node` bail. `on_error` is still
/// available — it lives on the calling `FlowStep`, not this node, and is
/// free to catch this sub-workflow failing as a whole.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowNode {
    /// Resolved relative to the directory containing the workflow file this
    /// node is defined in (not the current working directory, unlike
    /// `type: agent`'s `agent:`).
    pub(crate) workflow: PathBuf,
    pub(crate) jq: Option<String>,
    pub(crate) write_file: Option<PathBuf>,
}

/// `type: command` — runs `command[0]` as a child process with `command[1..]`
/// as its arguments, each rendered via `template::render` like `prompt` (this
/// never goes through a shell, so a rendered value can't inject an extra
/// argument or command the way string concatenation into a shell command
/// line could — see `docs/usage/ja/attachments.md`'s note on why `--file`
/// exists for the same reason). This node's current input is piped to the
/// process's stdin; its captured stdout (a single trailing newline stripped,
/// like a shell `$(...)` substitution) becomes this node's output, then goes
/// through `jq`/`write_file` like any other node's output. A non-UTF-8
/// stdout is rejected, matching `--file`'s text-only restriction. A non-zero
/// exit status fails this node's action (stderr included in the error), the
/// same as any other failure — subject to the calling `FlowStep`'s
/// `on_error` and this node's own `retry`. No model call, so no
/// `model`/sampling/`mcp`/`skills`/`subagents`/`system_prompt`/`files`/
/// `images`/schema fields.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandNode {
    pub(crate) command: Vec<String>,
    pub(crate) jq: Option<String>,
    pub(crate) write_file: Option<PathBuf>,
    pub(crate) retry: Option<RetryDefinition>,
    pub(crate) timeout: Option<u64>,
}

/// `type: transform` — a data-only node with no model call, agent, sub-
/// workflow, or command: `jq` reshapes the current input, `write_file` saves
/// it, or both. At least one of the two is required — the explicit form of
/// what used to be an implicit "no action fields set at all" case inferred
/// from a `NodeDefinition` with nothing else on it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransformNode {
    pub(crate) jq: Option<String>,
    pub(crate) write_file: Option<PathBuf>,
    pub(crate) retry: Option<RetryDefinition>,
    pub(crate) timeout: Option<u64>,
}

/// `type: ask` — a human-in-the-loop node: renders `prompt` (the same
/// handlebars template every other node's `prompt`/`system_prompt` uses,
/// against `{{ input }}`/`{{ steps.<id> }}`/`{{ vars.<key> }}`), prints it,
/// and reads the answer from stdin as this node's output. No model call, so
/// no `model`/sampling/`mcp`/`skills`/`subagents` fields — see
/// `workflow::ask::run_ask` for the actual read, and
/// `docs/usage/ja/workflow.md` for the non-interactive-stdin behavior.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AskNode {
    /// The question, rendered like `PromptNode::prompt`.
    pub(crate) prompt: String,
    /// Restricts the answer to one of these exact strings (after stripping
    /// the trailing newline the read itself already strips — no further
    /// trimming). An answer that doesn't match exactly is a runtime error;
    /// there is no re-prompt loop (stdin may not be interactive, or may be a
    /// script feeding fixed input, so looping could hang forever).
    pub(crate) choices: Option<Vec<String>>,
    /// Reads until EOF instead of a single line, for a multi-line answer
    /// (e.g. pasted text). Defaults to `false`.
    pub(crate) multiline: Option<bool>,
    /// Used as this node's output, without prompting, when stdin is not an
    /// interactive terminal (see `run_ask`). Required in that case — with
    /// neither an interactive terminal nor a `default:`, there is no way to
    /// get an answer, so the step fails instead of hanging.
    pub(crate) default: Option<String>,
    pub(crate) jq: Option<String>,
    pub(crate) write_file: Option<PathBuf>,
    pub(crate) retry: Option<RetryDefinition>,
    pub(crate) timeout: Option<u64>,
}

impl NodeDefinition {
    /// Returns the stable action kind represented by this node.
    pub(crate) fn kind(&self) -> NodeKind {
        match self {
            Self::Prompt(_) => NodeKind::Prompt,
            Self::Agent(_) => NodeKind::Agent,
            Self::Workflow(_) => NodeKind::Workflow,
            Self::Command(_) => NodeKind::Command,
            Self::Transform(_) => NodeKind::Transform,
            Self::Ask(_) => NodeKind::Ask,
        }
    }

    /// Whether this node's action is a model call that participates in the
    /// node > agent file > workflow `default:` sampling/capability/retry/
    /// timeout fallback chain (see `workflow::exec::resolve_step_settings`) —
    /// `Prompt`/`Agent` only. `Workflow`'s own fallback happens inside the
    /// sub-workflow's own steps instead (never on this node, which has no
    /// `retry`/`timeout`/sampling fields to fall back in the first place);
    /// `Command`/`Transform` make no model call at all, though either may
    /// still set its own `retry`/`timeout` explicitly — they just never
    /// inherit the workflow's `default.retry`/`default.timeout`.
    pub(crate) fn calls_model(&self) -> bool {
        self.kind().calls_model()
    }

    /// Returns the settings visible to generic node consumers. The view is
    /// borrowed from this definition and therefore does not clone any model
    /// aliases, capability lists, retry policy, or output paths.
    pub(crate) fn settings(&self) -> NodeSettings<'_> {
        match self {
            Self::Prompt(node) => NodeSettings {
                model: node.model.as_deref(),
                reasoning_effort: node.reasoning_effort,
                temperature: node.temperature,
                top_p: node.top_p,
                max_tokens: node.max_tokens,
                mcp: node.mcp.as_deref(),
                max_tool_rounds: node.max_tool_rounds,
                skills: node.skills.as_deref(),
                subagents: node.subagents.as_deref(),
                tools: node.tools.as_deref(),
                retry: node.retry.as_ref(),
                timeout: node.timeout,
                jq: node.jq.as_deref(),
                write_file: node.write_file.as_deref(),
            },
            Self::Agent(node) => NodeSettings {
                model: node.model.as_deref(),
                reasoning_effort: node.reasoning_effort,
                temperature: node.temperature,
                top_p: node.top_p,
                max_tokens: node.max_tokens,
                mcp: node.mcp.as_deref(),
                max_tool_rounds: node.max_tool_rounds,
                skills: node.skills.as_deref(),
                subagents: node.subagents.as_deref(),
                tools: node.tools.as_deref(),
                retry: node.retry.as_ref(),
                timeout: node.timeout,
                jq: node.jq.as_deref(),
                write_file: node.write_file.as_deref(),
            },
            Self::Workflow(node) => NodeSettings {
                jq: node.jq.as_deref(),
                write_file: node.write_file.as_deref(),
                ..NodeSettings::default()
            },
            Self::Command(node) => NodeSettings {
                retry: node.retry.as_ref(),
                timeout: node.timeout,
                jq: node.jq.as_deref(),
                write_file: node.write_file.as_deref(),
                ..NodeSettings::default()
            },
            Self::Transform(node) => NodeSettings {
                retry: node.retry.as_ref(),
                timeout: node.timeout,
                jq: node.jq.as_deref(),
                write_file: node.write_file.as_deref(),
                ..NodeSettings::default()
            },
            Self::Ask(node) => NodeSettings {
                retry: node.retry.as_ref(),
                timeout: node.timeout,
                jq: node.jq.as_deref(),
                write_file: node.write_file.as_deref(),
                ..NodeSettings::default()
            },
        }
    }

    /// This variant's `type:` name as it appears in a workflow YAML file
    /// (and in `lait run --dry-run`/`lait graph` output).
    pub(crate) fn type_name(&self) -> &'static str {
        self.kind().as_str()
    }

    /// Whether this node reads from the process's own stdin when it runs —
    /// `Ask` only. Such a node cannot safely run anywhere stdin isn't the
    /// single, sequential, human-facing stream a top-level step gets: inside
    /// a `parallel` branch or a concurrent `for_each` iteration, several
    /// instances would race to read the same stdin (see
    /// `validate::validate_steps`'s concurrency-safety checks, which reject
    /// this the same way they reject a concurrent `write_file`).
    pub(crate) fn requires_interactive_stdin(&self) -> bool {
        self.kind().requires_interactive_stdin()
    }
}

/// A control-flow reference site: one position in a `steps:` list. Carries
/// no action of its own — `use` points at a `NodeDefinition` in the
/// workflow's `nodes:` map, or one of `switch`/`parallel`/`loop`/`for_each`
/// routes to nested `steps` instead.
/// A step whose shape and nesting have passed workflow validation.
/// Construction is private; execution and presentation never receive wire shapes.
#[derive(Debug)]
pub(crate) struct FlowStep {
    id: Option<String>,
    action: StepAction,
}

#[derive(Debug)]
enum StepAction {
    Call {
        node: String,
        definition: Arc<NodeDefinition>,
        when: Option<String>,
        on_error: Option<OnErrorDefinition>,
        control: Control,
    },
    Switch(SwitchDefinition),
    Parallel(ParallelDefinition),
    Loop(LoopDefinition),
    ForEach(ForEachDefinition),
    Break {
        when: Option<String>,
    },
    Stop {
        when: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Control {
    Continue,
    Break,
    Stop,
}

pub(crate) enum Router<'a> {
    Switch(&'a SwitchDefinition),
    Parallel(&'a ParallelDefinition),
    Loop(&'a LoopDefinition),
    ForEach(&'a ForEachDefinition),
}

pub(crate) struct NodeCall<'a> {
    pub(crate) name: &'a str,
    pub(crate) definition: &'a NodeDefinition,
}

impl FlowStep {
    pub(crate) fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    pub(crate) fn call(&self) -> Option<NodeCall<'_>> {
        match &self.action {
            StepAction::Call {
                node, definition, ..
            } => Some(NodeCall {
                name: node,
                definition,
            }),
            _ => None,
        }
    }
    pub(crate) fn node_id(&self) -> Option<&str> {
        match &self.action {
            StepAction::Call { node, .. } => Some(node),
            _ => None,
        }
    }
    pub(crate) fn when(&self) -> Option<&str> {
        match &self.action {
            StepAction::Call { when, .. }
            | StepAction::Break { when }
            | StepAction::Stop { when } => when.as_deref(),
            _ => None,
        }
    }
    pub(crate) fn on_error(&self) -> Option<&OnErrorDefinition> {
        match &self.action {
            StepAction::Call { on_error, .. } => on_error.as_ref(),
            _ => None,
        }
    }
    pub(crate) fn control(&self) -> Control {
        match self.action {
            StepAction::Call { control, .. } => control,
            StepAction::Break { .. } => Control::Break,
            StepAction::Stop { .. } => Control::Stop,
            _ => Control::Continue,
        }
    }
    pub(crate) fn label(&self) -> Option<&str> {
        self.id().or_else(|| self.node_id())
    }
    pub(crate) fn label_or(&self, fallback: usize) -> String {
        self.label()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("step-{fallback}"))
    }
    pub(crate) fn router(&self) -> Option<Router<'_>> {
        match &self.action {
            StepAction::Switch(router) => Some(Router::Switch(router)),
            StepAction::Parallel(router) => Some(Router::Parallel(router)),
            StepAction::Loop(router) => Some(Router::Loop(router)),
            StepAction::ForEach(router) => Some(Router::ForEach(router)),
            StepAction::Call { .. } | StepAction::Break { .. } | StepAction::Stop { .. } => None,
        }
    }

    fn compile(raw: super::raw::FlowStep, nodes: &NodeMap) -> Result<Self> {
        let control = if raw.stop == Some(true) {
            Control::Stop
        } else if raw.r#break == Some(true) {
            Control::Break
        } else {
            Control::Continue
        };
        let action = if let Some(node) = raw.r#use {
            StepAction::Call {
                definition: Arc::clone(
                    nodes
                        .get(&node)
                        .with_context(|| format!("unknown workflow node '{node}'"))?,
                ),
                node,
                when: raw.when,
                on_error: raw
                    .on_error
                    .map(|handler| {
                        Ok::<_, anyhow::Error>(OnErrorDefinition {
                            steps: compile_steps(handler.steps, nodes)?,
                        })
                    })
                    .transpose()?,
                control,
            }
        } else if let Some(router) = raw.switch {
            StepAction::Switch(SwitchDefinition {
                cases: router
                    .cases
                    .into_iter()
                    .map(|case| {
                        Ok::<_, anyhow::Error>(CaseDefinition {
                            id: case.id,
                            when: case.when,
                            steps: compile_steps(case.steps, nodes)?,
                        })
                    })
                    .collect::<Result<_>>()?,
                else_steps: router
                    .else_steps
                    .map(|steps| compile_steps(steps, nodes))
                    .transpose()?,
            })
        } else if let Some(router) = raw.parallel {
            StepAction::Parallel(ParallelDefinition {
                branches: router
                    .branches
                    .into_iter()
                    .map(|branch| {
                        Ok::<_, anyhow::Error>(BranchDefinition {
                            id: branch.id,
                            steps: compile_steps(branch.steps, nodes)?,
                        })
                    })
                    .collect::<Result<_>>()?,
                join: router.join,
            })
        } else if let Some(router) = raw.r#loop {
            StepAction::Loop(LoopDefinition {
                condition: match (router.r#while, router.until) {
                    (Some(filter), None) => LoopCondition::While(filter),
                    (None, Some(filter)) => LoopCondition::Until(filter),
                    _ => anyhow::bail!("loop requires exactly one condition"),
                },
                max_iterations: router
                    .max_iterations
                    .and_then(std::num::NonZeroUsize::new)
                    .context("loop requires a positive max_iterations")?,
                steps: compile_steps(router.steps, nodes)?,
            })
        } else if let Some(router) = raw.for_each {
            StepAction::ForEach(ForEachDefinition {
                items: router.items,
                steps: compile_steps(router.steps, nodes)?,
                join: router.join,
                max_concurrency: router.max_concurrency,
            })
        } else if raw.stop == Some(true) {
            StepAction::Stop { when: raw.when }
        } else if raw.r#break == Some(true) {
            StepAction::Break { when: raw.when }
        } else {
            anyhow::bail!("step has no action or active control directive");
        };
        Ok(Self { id: raw.id, action })
    }
}

fn compile_steps(steps: Vec<super::raw::FlowStep>, nodes: &NodeMap) -> Result<Vec<FlowStep>> {
    steps
        .into_iter()
        .map(|step| FlowStep::compile(step, nodes))
        .collect()
}

impl WorkflowDocument<super::raw::FlowStep> {
    /// The only raw-to-executable conversion validates the entire document
    /// before binding node references and compiling private action variants.
    pub(super) fn validate(self) -> Result<WorkflowFile> {
        if let Some(version) = self.version
            && version != CURRENT_WORKFLOW_VERSION
        {
            anyhow::bail!(
                "unsupported workflow schema 'version: {version}'; this build of lait supports \
             version {CURRENT_WORKFLOW_VERSION} (omit 'version:' to use the latest one this \
             build supports)"
            );
        }
        if self.steps.is_empty() {
            anyhow::bail!("workflow must contain at least one step");
        }
        super::validate::validate_workflow_defaults(&self.default)?;
        for (node_id, node) in &self.nodes {
            super::validate::validate_node(node, node_id)?;
        }
        super::validate::validate_steps(
            &self.steps,
            &self.nodes,
            super::validate::FlowContext::TOP_LEVEL,
        )?;

        let steps = compile_steps(self.steps, &self.nodes)?;
        Ok(WorkflowDocument {
            version: self.version,
            name: self.name,
            description: self.description,
            default: self.default,
            models: self.models,
            json_schemas: self.json_schemas,
            nodes: self.nodes,
            steps,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetryDefinition {
    /// The total number of attempts, including the first (i.e. `3` means "try
    /// once, then retry up to twice more"). Required, and must be at least 1.
    pub(crate) max_attempts: Option<usize>,
    /// How long to wait, in seconds, before the first retry (after attempt 1
    /// fails). Defaults to 0 (retry immediately).
    pub(crate) delay_seconds: Option<u64>,
    /// Multiplies the wait before each subsequent retry (e.g. `2.0` doubles
    /// it every time). Defaults to `1.0` (a constant delay).
    pub(crate) backoff: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OnErrorDefinition<S = FlowStep> {
    /// Run once, with the failure's `{"error": ..., "input": ...}` object as
    /// `{{ input }}`, in place of failing the workflow. `stop`/`break` are
    /// allowed here like anywhere else (subject to the same nesting rules as
    /// the failing step itself).
    pub(crate) steps: Vec<S>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SwitchDefinition<S = FlowStep> {
    /// Evaluated in order; the first case whose `when` is truthy runs.
    pub(crate) cases: Vec<CaseDefinition<S>>,
    /// Runs when no `case` matched. Required unless the workflow author is
    /// sure `cases` is exhaustive: a `switch` with no matching case and no
    /// `else` is a runtime error rather than a silent pass-through.
    #[serde(rename = "else")]
    pub(crate) else_steps: Option<Vec<S>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaseDefinition<S = FlowStep> {
    /// An optional label used only in progress output (like `FlowStep::id`).
    pub(crate) id: Option<String>,
    /// A jq filter evaluated against the current input; see `FlowStep::when`.
    pub(crate) when: String,
    pub(crate) steps: Vec<S>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParallelDefinition<S = FlowStep> {
    /// Every branch runs concurrently against the same input (a snapshot of
    /// `{{ input }}` as it stood when the `parallel` step started). Their
    /// outputs are collected, in `branches` declaration order (not
    /// completion order, so the join is deterministic), into a JSON object
    /// keyed by each branch's `id` (or its default label; see
    /// `BranchDefinition::label`).
    pub(crate) branches: Vec<BranchDefinition<S>>,
    /// A jq filter applied to that id-keyed object, the same way a node's
    /// own `jq` applies to its output. If omitted, the object itself
    /// (serialized as JSON) becomes `{{ input }}` for the next step.
    pub(crate) join: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchDefinition<S = FlowStep> {
    /// Defaults to `branch-{n}` (1-based), like `FlowStep::id`. Unlike
    /// a step or case id, this also becomes the branch's key in the joined
    /// JSON object, so it must be unique within its `parallel`.
    pub(crate) id: Option<String>,
    pub(crate) steps: Vec<S>,
}

impl<S> BranchDefinition<S> {
    /// The label used both for progress output and as the branch's key in
    /// the joined JSON object. `index` is 0-based.
    pub(crate) fn label(&self, index: usize) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("branch-{}", index + 1))
    }
}

#[derive(Debug)]
pub(crate) struct LoopDefinition {
    pub(crate) condition: LoopCondition,
    pub(crate) max_iterations: std::num::NonZeroUsize,
    pub(crate) steps: Vec<FlowStep>,
}

#[derive(Debug)]
pub(crate) enum LoopCondition {
    While(String),
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawLoopDefinition<S> {
    /// Checked before each iteration (including the first), against the
    /// current input; the loop runs while this is truthy, so it may run zero
    /// times. Mutually exclusive with `until`; exactly one of them is
    /// required.
    pub(crate) r#while: Option<String>,
    /// Checked after each iteration, against that iteration's output; the
    /// loop stops once this becomes truthy, so `steps` always runs at least
    /// once. Mutually exclusive with `while`; exactly one of them is
    /// required. Note the condition runs through the same JSON-or-string
    /// coercion as `when` (see `eval_when`), so the last step in `steps`
    /// must produce a value it can be evaluated against (via
    /// `output_schema` or `jq`) for anything beyond a plain truthy/falsy
    /// text check.
    pub(crate) until: Option<String>,
    /// Safety cap on the number of iterations. Required (and must be at
    /// least 1): reaching it without `while`/`until` being satisfied is a
    /// runtime error rather than a silent stop, so this is an assertion
    /// ("must finish within N iterations"), not just a safety valve.
    pub(crate) max_iterations: Option<usize>,
    /// The loop body, re-run each iteration. Each iteration's final output
    /// becomes `{{ input }}` for the next iteration (or, for the first
    /// iteration, this is the `loop` step's own incoming input).
    pub(crate) steps: Vec<S>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForEachDefinition<S = FlowStep> {
    /// A jq filter evaluated once against the current input; must produce
    /// exactly one output value, which must be a JSON array (e.g. `.items`,
    /// not a stream-producing filter like `.items[]`). Each element becomes
    /// one iteration's `{{ input }}`; unlike a `parallel` branch, the body
    /// cannot see anything of the surrounding input beyond that element.
    pub(crate) items: String,
    /// The loop body, run once per element of `items`, in array order.
    pub(crate) steps: Vec<S>,
    /// A jq filter applied to the JSON array of per-element outputs (in
    /// `items` order), the same way `ParallelDefinition::join` applies to
    /// the id-keyed object from a `parallel` step. If omitted, the array
    /// itself (serialized as JSON) becomes `{{ input }}` for the next step.
    pub(crate) join: Option<String>,
    /// The maximum number of items processed concurrently. Defaults to `1`
    /// (fully sequential, the original behavior); when greater than `1`,
    /// `steps` runs like a `parallel` branch per item (its own
    /// `{{ steps.* }}`/`$steps` recordings stay item-local, and `stop`/
    /// `break` are rejected inside it) rather than like a sequential `loop`
    /// iteration. Must be at least `1`.
    pub(crate) max_concurrency: Option<usize>,
}
