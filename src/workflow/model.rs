use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

use crate::{cli::ReasoningEffort, config::ModelMap, schema::JsonSchemaMap};

/// The workflow-file-scoped map of reusable action definitions, keyed by the
/// name used in `steps[].use`. Unlike `models`/`json_schemas`, this is never
/// merged into a nested `workflow:` step's sub-workflow scope — each file's
/// `use:` resolves only against its own `nodes:` (see `WorkflowScope::nodes`
/// in `app.rs`).
pub(crate) type NodeMap = BTreeMap<String, NodeDefinition>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowFile {
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
#[derive(Debug, Default, Deserialize)]
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
}

/// A reusable action definition, referenced by id from `steps[].use`. Carries
/// only "what to do" — model call or data transform — never "when"/"how many
/// times", which lives on the `FlowStep` reference site instead.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeDefinition {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Sampling temperature (0.0-2.0) for this node's model call. Falls back
    /// independently to the workflow's `default.temperature` when unset (like
    /// `reasoning_effort`, not like `retry`'s whole-unit fallback). Only
    /// meaningful for a node that calls a model (`prompt`/`agent`).
    pub(crate) temperature: Option<f64>,
    /// Nucleus sampling probability mass (0.0-1.0) for this node's model
    /// call. Falls back independently to `default.top_p`, like `temperature`.
    pub(crate) top_p: Option<f64>,
    /// An upper bound on the number of tokens generated for this node's
    /// model call. Falls back independently to `default.max_tokens`, like
    /// `temperature`.
    pub(crate) max_tokens: Option<u32>,
    /// The prompt template sent to the model. A node without a `prompt` and
    /// without an `agent` does not call the model at all; it must then have a
    /// `jq` filter, making it a data-only transformation node. Mutually
    /// exclusive with `agent`.
    pub(crate) prompt: Option<String>,
    /// Path to an agent Markdown file (see `agent::load_agent`) whose system
    /// prompt, model/reasoning defaults, and input/output schema drive this
    /// node instead of `prompt`/`input_schema`/`output_schema`/`schema_name`.
    /// Mutually exclusive with `prompt`, `input_schema`, `output_schema`, and
    /// `schema_name`.
    pub(crate) agent: Option<PathBuf>,
    /// Path to another workflow YAML file (resolved relative to the
    /// directory containing the workflow file this node is defined in, not
    /// the current working directory), run against this node's input; that
    /// sub-workflow's final output becomes this node's output. Its own
    /// `default:`/`models:`/`json_schemas:` take precedence, falling back to
    /// this workflow's when it doesn't define an entry. Mutually exclusive
    /// with `prompt`, `agent`, `model`, `reasoning_effort`,
    /// `temperature`/`top_p`/`max_tokens`,
    /// `input_schema`/`output_schema`/`schema_name` (which the sub-workflow's
    /// own steps supply), and `retry`/`timeout` (set those on the
    /// sub-workflow's own steps instead). `on_error` is not excluded — it
    /// lives on the calling `FlowStep`, not this node, and is free to catch
    /// this sub-workflow failing as a whole.
    pub(crate) workflow: Option<PathBuf>,
    /// Validates this node's input before it runs (before rendering `prompt`,
    /// or before `jq` for a transform-only node). Resolved against the
    /// workflow's top-level `json_schemas:` first; if no such key exists,
    /// treated as a path to a JSON schema file instead. Mutually exclusive
    /// with `agent`, whose agent file supplies its own `input_schema`.
    pub(crate) input_schema: Option<String>,
    /// Request a structured JSON response using the named schema, like the CLI's
    /// `--json-schema`. Resolved against the workflow's top-level `json_schemas:`
    /// first; if no such key exists, treated as a path to a JSON schema file
    /// instead. Requires `prompt`.
    pub(crate) output_schema: Option<String>,
    /// The name of the structured output schema. Defaults to `structured_output`,
    /// like the CLI's `--schema-name`. Only used together with `output_schema`.
    pub(crate) schema_name: Option<String>,
    /// A jq filter applied to this node's output (the model's response, or the
    /// running input if there is no `prompt`) before it becomes `{{ input }}` for
    /// the next step. The filtered value must be valid JSON.
    pub(crate) jq: Option<String>,
    /// Writes this node's final output (after `jq`, if set) to this path,
    /// overwriting it if it already exists (parent directories are not
    /// created automatically). Resolved relative to the current working
    /// directory, like `agent:` (not relative to the workflow file, unlike
    /// `workflow:`). Does not change what becomes `{{ input }}` for the next
    /// step. Rejected on a node used inside a `for_each` body whose
    /// `max_concurrency` is above 1, where every concurrently running item
    /// would write the same static path.
    pub(crate) write_file: Option<PathBuf>,
    /// Retries this node's action (`input_schema` check, `prompt`/`agent`
    /// call, and `jq`, as one unit) up to `max_attempts` times on failure.
    /// Applies before the calling `FlowStep`'s `on_error`, which only runs
    /// once every attempt here has failed. Falls back to the workflow's
    /// `default.retry` (as a whole struct, not merged field-by-field) when
    /// unset on a node that calls a model (`prompt`/`agent`); a `jq`-only or
    /// `workflow:` node never retries on its own account.
    pub(crate) retry: Option<RetryDefinition>,
    /// A per-attempt time limit, in seconds, on this node's action. A timed
    /// out attempt counts as a failure for `retry`, the same as any other
    /// error. Falls back to the workflow's `default.timeout` under the same
    /// rule as `retry` above.
    pub(crate) timeout: Option<u64>,
    /// Names of `mcp_servers:` entries (from `lait.config.yml`) whose tools
    /// this node's model call may use. Only meaningful for a node that calls
    /// a model (`prompt`/`agent`); falls back to the agent file's own `mcp:`
    /// (for an `agent` node), then to the workflow's `default.mcp`, the same
    /// way as `reasoning_effort`.
    pub(crate) mcp: Option<Vec<String>>,
    /// The maximum number of tool-call round trips this node's model call may
    /// take before lait errors, when `mcp` (from any fallback layer) names at
    /// least one server. Falls back the same way as `mcp`.
    pub(crate) max_tool_rounds: Option<usize>,
    /// Names of `skills:` entries (from `lait.config.yml`) whose content is
    /// appended to this node's system prompt. Only meaningful for a node that
    /// calls a model (`prompt`/`agent`); falls back to the agent file's own
    /// `skills:` (for an `agent` node), then to the workflow's
    /// `default.skills`, the same way as `mcp`.
    pub(crate) skills: Option<Vec<String>>,
    /// Names of `agents:` entries (from `lait.config.yml`) made available as
    /// callable subagent tools during this node's model call. Only
    /// meaningful for a node that calls a model (`prompt`/`agent`); falls
    /// back to the agent file's own `subagents:` (for an `agent` node), then
    /// to the workflow's `default.subagents`, the same way as `mcp`.
    pub(crate) subagents: Option<Vec<String>>,
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
    /// (after its own `prompt`/`agent`/`jq` action, if any), using its output
    /// as the workflow's final result; no further steps run. Rejected inside
    /// a `parallel` branch, where concurrently running sibling branches make
    /// "stop the workflow" ambiguous. Mutually exclusive with `break`. May
    /// accompany `use` (checked after the node runs and its output is
    /// recorded), or stand alone.
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
