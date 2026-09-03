use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

use crate::{cli::ReasoningEffort, config::ModelMap, schema::JsonSchemaMap};

/// The only workflow schema version this build understands. `WorkflowFile`'s
/// `version:` is optional (omitted means "latest"); an explicit but
/// unrecognized number is rejected outright rather than silently misparsed
/// — see `super::parse_workflow`.
pub(crate) const CURRENT_WORKFLOW_VERSION: u32 = 1;

/// The workflow-file-scoped map of reusable action definitions, keyed by the
/// name used in `steps[].use`. Unlike `models`/`json_schemas`, this is never
/// merged into a nested `workflow:` step's sub-workflow scope — each file's
/// `use:` resolves only against its own `nodes:` (see `WorkflowScope::nodes`
/// in `app.rs`).
pub(crate) type NodeMap = BTreeMap<String, NodeDefinition>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowFile {
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
    pub(crate) steps: Vec<FlowStep>,
}

/// A workflow file's `default:` block: the same `model`/`reasoning_effort`
/// fallback as `lait.config.yml`'s `default:` (see
/// `config::DefaultSettings`), plus a workflow-only `retry`/`timeout`
/// fallback applied to any step that calls a model (`prompt`/`agent`) and
/// doesn't set its own (see `NodeDefinition::retry`/`timeout`). Kept as its
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
/// cost far more than the duplication does. `NodeDefinition`'s own methods
/// below (`model`/`jq`/`retry`/... ) read through to whichever variant has
/// that field, `None` for one that doesn't, so most call sites outside
/// `workflow::validate`/`workflow::exec` never have to match on the variant
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
        matches!(self, NodeDefinition::Prompt(_) | NodeDefinition::Agent(_))
    }

    pub(crate) fn model(&self) -> Option<&str> {
        match self {
            NodeDefinition::Prompt(node) => node.model.as_deref(),
            NodeDefinition::Agent(node) => node.model.as_deref(),
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    pub(crate) fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        match self {
            NodeDefinition::Prompt(node) => node.reasoning_effort,
            NodeDefinition::Agent(node) => node.reasoning_effort,
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    pub(crate) fn temperature(&self) -> Option<f64> {
        match self {
            NodeDefinition::Prompt(node) => node.temperature,
            NodeDefinition::Agent(node) => node.temperature,
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    pub(crate) fn top_p(&self) -> Option<f64> {
        match self {
            NodeDefinition::Prompt(node) => node.top_p,
            NodeDefinition::Agent(node) => node.top_p,
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    pub(crate) fn max_tokens(&self) -> Option<u32> {
        match self {
            NodeDefinition::Prompt(node) => node.max_tokens,
            NodeDefinition::Agent(node) => node.max_tokens,
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    pub(crate) fn mcp(&self) -> Option<&[String]> {
        match self {
            NodeDefinition::Prompt(node) => node.mcp.as_deref(),
            NodeDefinition::Agent(node) => node.mcp.as_deref(),
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    pub(crate) fn max_tool_rounds(&self) -> Option<usize> {
        match self {
            NodeDefinition::Prompt(node) => node.max_tool_rounds,
            NodeDefinition::Agent(node) => node.max_tool_rounds,
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    pub(crate) fn skills(&self) -> Option<&[String]> {
        match self {
            NodeDefinition::Prompt(node) => node.skills.as_deref(),
            NodeDefinition::Agent(node) => node.skills.as_deref(),
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    pub(crate) fn subagents(&self) -> Option<&[String]> {
        match self {
            NodeDefinition::Prompt(node) => node.subagents.as_deref(),
            NodeDefinition::Agent(node) => node.subagents.as_deref(),
            NodeDefinition::Workflow(_)
            | NodeDefinition::Command(_)
            | NodeDefinition::Transform(_)
            | NodeDefinition::Ask(_) => None,
        }
    }

    /// `retry`/`timeout` have no fallback of their own (unlike sampling/
    /// capability knobs above): `Workflow` has neither field at all, and
    /// `Command`/`Transform` may set their own but never inherit
    /// `default.retry`/`default.timeout` (see `calls_model`'s doc comment).
    pub(crate) fn retry(&self) -> Option<&RetryDefinition> {
        match self {
            NodeDefinition::Prompt(node) => node.retry.as_ref(),
            NodeDefinition::Agent(node) => node.retry.as_ref(),
            NodeDefinition::Workflow(_) => None,
            NodeDefinition::Command(node) => node.retry.as_ref(),
            NodeDefinition::Transform(node) => node.retry.as_ref(),
            NodeDefinition::Ask(node) => node.retry.as_ref(),
        }
    }

    pub(crate) fn timeout(&self) -> Option<u64> {
        match self {
            NodeDefinition::Prompt(node) => node.timeout,
            NodeDefinition::Agent(node) => node.timeout,
            NodeDefinition::Workflow(_) => None,
            NodeDefinition::Command(node) => node.timeout,
            NodeDefinition::Transform(node) => node.timeout,
            NodeDefinition::Ask(node) => node.timeout,
        }
    }

    /// A jq filter applied to this node's output, common to every variant.
    pub(crate) fn jq(&self) -> Option<&str> {
        match self {
            NodeDefinition::Prompt(node) => node.jq.as_deref(),
            NodeDefinition::Agent(node) => node.jq.as_deref(),
            NodeDefinition::Workflow(node) => node.jq.as_deref(),
            NodeDefinition::Command(node) => node.jq.as_deref(),
            NodeDefinition::Transform(node) => node.jq.as_deref(),
            NodeDefinition::Ask(node) => node.jq.as_deref(),
        }
    }

    /// Where this node's output (after `jq`, if set) is written, common to
    /// every variant.
    pub(crate) fn write_file(&self) -> Option<&std::path::Path> {
        match self {
            NodeDefinition::Prompt(node) => node.write_file.as_deref(),
            NodeDefinition::Agent(node) => node.write_file.as_deref(),
            NodeDefinition::Workflow(node) => node.write_file.as_deref(),
            NodeDefinition::Command(node) => node.write_file.as_deref(),
            NodeDefinition::Transform(node) => node.write_file.as_deref(),
            NodeDefinition::Ask(node) => node.write_file.as_deref(),
        }
    }

    /// This variant's `type:` name as it appears in a workflow YAML file
    /// (and in `lait run --dry-run`/`lait graph` output).
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            NodeDefinition::Prompt(_) => "prompt",
            NodeDefinition::Agent(_) => "agent",
            NodeDefinition::Workflow(_) => "workflow",
            NodeDefinition::Command(_) => "command",
            NodeDefinition::Transform(_) => "transform",
            NodeDefinition::Ask(_) => "ask",
        }
    }

    /// Whether this node reads from the process's own stdin when it runs —
    /// `Ask` only. Such a node cannot safely run anywhere stdin isn't the
    /// single, sequential, human-facing stream a top-level step gets: inside
    /// a `parallel` branch or a concurrent `for_each` iteration, several
    /// instances would race to read the same stdin (see
    /// `validate::validate_steps`'s concurrency-safety checks, which reject
    /// this the same way they reject a concurrent `write_file`).
    pub(crate) fn requires_interactive_stdin(&self) -> bool {
        matches!(self, NodeDefinition::Ask(_))
    }
}

/// A control-flow reference site: one position in a `steps:` list. Carries
/// no action of its own — `use` points at a `NodeDefinition` in the
/// workflow's `nodes:` map, or one of `switch`/`parallel`/`loop`/`for_each`
/// routes to nested `steps` instead.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FlowStep {
    /// This site's label, used for progress output and as the key this
    /// site's output is recorded under in `{{ steps.<id> }}`/`$steps`.
    /// Defaults to the referenced node's id (see `FlowStep::label`). Required
    /// to differ from any node id it does not itself reference, since node
    /// ids and site ids share the same `$steps` namespace (see
    /// `validate::validate_steps`).
    pub(crate) id: Option<String>,
    /// The id of the node (in the workflow's `nodes:` map) this site runs.
    /// Mutually exclusive with `switch`/`parallel`/`loop`/`for_each`; exactly
    /// one of the five, or none of them together with `stop`/`break`, is
    /// required.
    #[serde(rename = "use")]
    pub(crate) r#use: Option<String>,
    /// A jq filter evaluated against the current input (JSON-parsed, falling
    /// back to a JSON string for plain text, like `template::parse_input`).
    /// A falsy result (`false`/`null`) skips this site entirely, passing the
    /// input through unchanged to the next step. Only meaningful together
    /// with `use`.
    pub(crate) when: Option<String>,
    /// Runs in place of failing the workflow when this site's node (after
    /// every `retry` attempt, if any) still fails. Only meaningful together
    /// with `use`.
    pub(crate) on_error: Option<OnErrorDefinition>,
    /// Turns this site into a branch router: evaluates `cases` in order and
    /// runs the first one whose `when` is truthy (or `else`, if none match).
    /// Mutually exclusive with every other field except `id`.
    pub(crate) switch: Option<SwitchDefinition>,
    /// Turns this site into a fan-out/fan-in: runs every branch concurrently
    /// against the same input and joins their outputs. Mutually exclusive
    /// with every other field except `id`.
    pub(crate) parallel: Option<ParallelDefinition>,
    /// Turns this site into a conditional loop: re-runs `steps` while/until a
    /// jq condition holds, threading each iteration's output into the next
    /// iteration's `{{ input }}`. Mutually exclusive with every other field
    /// except `id`.
    pub(crate) r#loop: Option<LoopDefinition>,
    /// Turns this site into an array map: runs `steps` once per element of a
    /// jq-selected array, collecting the results (in array order) into a
    /// JSON array. Mutually exclusive with every other field except `id`.
    pub(crate) for_each: Option<ForEachDefinition>,
    /// Ends the workflow successfully right after this site's node runs
    /// (after its own action, if any), using its output as the workflow's
    /// final result; no further steps run. Rejected inside a `parallel`
    /// branch, where concurrently running sibling branches make "stop the
    /// workflow" ambiguous. Mutually exclusive with `break`. May accompany
    /// `use` (checked after the node runs and its output is recorded), or
    /// stand alone.
    pub(crate) stop: Option<bool>,
    /// Exits the nearest enclosing `loop`/`for_each` body right after this
    /// site's node runs, using its output as that iteration's result (the
    /// loop then proceeds as if the iteration had finished normally, i.e.
    /// checking `while`/`until` or moving to `join`). Requires an enclosing
    /// `loop`/`for_each` reachable without crossing a `parallel` branch
    /// boundary. Mutually exclusive with `stop`. May accompany `use`, or
    /// stand alone.
    pub(crate) r#break: Option<bool>,
}

impl FlowStep {
    /// This site's label for progress output and `$steps` recording: its own
    /// `id` if set, else the referenced node's id (for a `use` site), else
    /// `None` (a router site with no `id`, whose caller falls back to a
    /// `step-N` counter label).
    pub(crate) fn label(&self) -> Option<&str> {
        self.id.as_deref().or(self.r#use.as_deref())
    }

    /// `label()`, falling back to `step-<fallback_n>` when this site has
    /// neither an explicit `id` nor a `use` to name it. Shared by
    /// `run_steps`' progress labels and `validate_steps`' error labels, so
    /// both name a given site the same way.
    pub(crate) fn label_or(&self, fallback_n: usize) -> String {
        self.label()
            .map(str::to_string)
            .unwrap_or_else(|| format!("step-{fallback_n}"))
    }
}

/// The step kinds that route to nested `steps` instead of acting directly on
/// their own input, borrowed out of whichever of `FlowStep::switch`/
/// `parallel`/`loop`/`for_each` is set. See `FlowStep::router`.
pub(crate) enum Router<'a> {
    Switch(&'a SwitchDefinition),
    Parallel(&'a ParallelDefinition),
    Loop(&'a LoopDefinition),
    ForEach(&'a ForEachDefinition),
}

impl FlowStep {
    /// Which router kind this site is, if any. `validate::validate_steps`
    /// checks `switch`/`parallel`/`loop`/`for_each` are not set together
    /// before ever calling this (see its `router_count` check), so checking
    /// them in a fixed order here is safe; called on a site that hasn't been
    /// through that check, it would silently prefer the first one set.
    /// `validate_steps` and `run_steps` both match on this so a new router
    /// kind requires updating both.
    pub(crate) fn router(&self) -> Option<Router<'_>> {
        if let Some(switch) = &self.switch {
            return Some(Router::Switch(switch));
        }
        if let Some(parallel) = &self.parallel {
            return Some(Router::Parallel(parallel));
        }
        if let Some(loop_def) = &self.r#loop {
            return Some(Router::Loop(loop_def));
        }
        self.for_each.as_ref().map(Router::ForEach)
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
pub(crate) struct OnErrorDefinition {
    /// Run once, with the failure's `{"error": ..., "input": ...}` object as
    /// `{{ input }}`, in place of failing the workflow. `stop`/`break` are
    /// allowed here like anywhere else (subject to the same nesting rules as
    /// the failing step itself).
    pub(crate) steps: Vec<FlowStep>,
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
    pub(crate) else_steps: Option<Vec<FlowStep>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaseDefinition {
    /// An optional label used only in progress output (like `FlowStep::id`).
    pub(crate) id: Option<String>,
    /// A jq filter evaluated against the current input; see `FlowStep::when`.
    pub(crate) when: String,
    pub(crate) steps: Vec<FlowStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParallelDefinition {
    /// Every branch runs concurrently against the same input (a snapshot of
    /// `{{ input }}` as it stood when the `parallel` step started). Their
    /// outputs are collected, in `branches` declaration order (not
    /// completion order, so the join is deterministic), into a JSON object
    /// keyed by each branch's `id` (or its default label; see
    /// `BranchDefinition::label`).
    pub(crate) branches: Vec<BranchDefinition>,
    /// A jq filter applied to that id-keyed object, the same way a node's
    /// own `jq` applies to its output. If omitted, the object itself
    /// (serialized as JSON) becomes `{{ input }}` for the next step.
    pub(crate) join: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchDefinition {
    /// Defaults to `branch-{n}` (1-based), like `FlowStep::id`. Unlike
    /// a step or case id, this also becomes the branch's key in the joined
    /// JSON object, so it must be unique within its `parallel`.
    pub(crate) id: Option<String>,
    pub(crate) steps: Vec<FlowStep>,
}

impl BranchDefinition {
    /// The label used both for progress output and as the branch's key in
    /// the joined JSON object. `index` is 0-based.
    pub(crate) fn label(&self, index: usize) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("branch-{}", index + 1))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoopDefinition {
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
    pub(crate) steps: Vec<FlowStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForEachDefinition {
    /// A jq filter evaluated once against the current input; must produce
    /// exactly one output value, which must be a JSON array (e.g. `.items`,
    /// not a stream-producing filter like `.items[]`). Each element becomes
    /// one iteration's `{{ input }}`; unlike a `parallel` branch, the body
    /// cannot see anything of the surrounding input beyond that element.
    pub(crate) items: String,
    /// The loop body, run once per element of `items`, in array order.
    pub(crate) steps: Vec<FlowStep>,
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
