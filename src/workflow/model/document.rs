//! The workflow document shape and its `default:` block. Split out of
//! `workflow/model.rs` — see that module's former doc comment, now on
//! [`super`].

use serde::Deserialize;

use crate::{config::ModelMap, reasoning::ReasoningEffort, schema::JsonSchemaMap};

use super::{NodeMap, flow::RetryDefinition};

/// The only workflow schema version this build understands. `WorkflowFile`'s
/// `version:` is optional (omitted means "latest"); an explicit but
/// unrecognized number is rejected outright rather than silently misparsed
/// — see `super::parse_workflow`.
pub(crate) const CURRENT_WORKFLOW_VERSION: u32 = 1;

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
