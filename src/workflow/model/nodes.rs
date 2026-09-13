//! Node types: what a workflow's `nodes:` map holds, and the borrowed
//! [`NodeSettings`] view execution/validation/presentation share instead of
//! repeating a six-variant match. Split out of `workflow/model.rs` — see
//! that module's former doc comment, now on [`super`].

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::reasoning::ReasoningEffort;

use super::flow::RetryDefinition;

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

/// `NodeSettings` for a node kind that has only `retry`/`timeout`/`jq`/
/// `write_file` and no model/capability fields — `CommandNode`,
/// `TransformNode`, and `AskNode` share this exact field set (same names,
/// same types) but are three distinct structs, so `NodeDefinition::settings`
/// can't match them together with a single `|` pattern; this helper keeps
/// their three otherwise-identical match arms to one call each instead of
/// three near-identical struct literals.
fn control_only_settings<'a>(
    retry: Option<&'a RetryDefinition>,
    timeout: Option<u64>,
    jq: Option<&'a str>,
    write_file: Option<&'a Path>,
) -> NodeSettings<'a> {
    NodeSettings {
        retry,
        timeout,
        jq,
        write_file,
        ..NodeSettings::default()
    }
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
            Self::Command(node) => control_only_settings(
                node.retry.as_ref(),
                node.timeout,
                node.jq.as_deref(),
                node.write_file.as_deref(),
            ),
            Self::Transform(node) => control_only_settings(
                node.retry.as_ref(),
                node.timeout,
                node.jq.as_deref(),
                node.write_file.as_deref(),
            ),
            Self::Ask(node) => control_only_settings(
                node.retry.as_ref(),
                node.timeout,
                node.jq.as_deref(),
                node.write_file.as_deref(),
            ),
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
