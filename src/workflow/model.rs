use std::path::PathBuf;

use serde::Deserialize;

use crate::{
    cli::ReasoningEffort,
    config::{DefaultSettings, ModelMap},
    schema::JsonSchemaMap,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowFile {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) default: DefaultSettings,
    /// Model aliases usable by `default.model`/`steps[].model`, in the same shape as
    /// `lait.config.yml`'s top-level `models:`. Takes precedence over an alias of
    /// the same name defined in `lait.config.yml`.
    #[serde(default)]
    pub(crate) models: ModelMap,
    /// Named schema definitions usable by `steps[].output_schema` and
    /// `steps[].input_schema`, each either a `file_path:` to a JSON schema
    /// file or an inline `schema:` body.
    #[serde(default)]
    pub(crate) json_schemas: JsonSchemaMap,
    pub(crate) steps: Vec<StepDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepDefinition {
    pub(crate) id: Option<String>,
    /// A jq filter evaluated against the current input (JSON-parsed, falling
    /// back to a JSON string for plain text, like `template::parse_input`).
    /// A falsy result (`false`/`null`) skips this step entirely, passing the
    /// input through unchanged to the next step. Mutually exclusive with
    /// `switch`.
    pub(crate) when: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// The prompt template sent to the model. A step without a `prompt` and
    /// without an `agent` does not call the model at all; it must then have a
    /// `jq` filter, making it a data-only transformation step. Mutually
    /// exclusive with `agent`.
    pub(crate) prompt: Option<String>,
    /// Path to an agent Markdown file (see `agent::load_agent`) whose system
    /// prompt, model/reasoning defaults, and input/output schema drive this
    /// step instead of `prompt`/`input_schema`/`output_schema`/`schema_name`.
    /// Mutually exclusive with `prompt`, `input_schema`, `output_schema`, and
    /// `schema_name`.
    pub(crate) agent: Option<PathBuf>,
    /// Path to another workflow YAML file (resolved relative to the
    /// directory containing the workflow file this step is defined in, not
    /// the current working directory), run against this step's input; that
    /// sub-workflow's final output becomes this step's output. Its own
    /// `default:`/`models:`/`json_schemas:` take precedence, falling back to
    /// this workflow's when it doesn't define an entry. Mutually exclusive
    /// with `prompt`, `agent`, `model`, `reasoning_effort`,
    /// `input_schema`/`output_schema`/`schema_name` (which the sub-workflow's
    /// own steps supply), and `retry`/`timeout`/`on_error` (set those on the
    /// sub-workflow's own steps instead).
    pub(crate) workflow: Option<PathBuf>,
    /// Validates this step's input before it runs (before rendering `prompt`,
    /// or before `jq` for a transform-only step). Resolved against the
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
    /// A jq filter applied to this step's output (the model's response, or the
    /// running input if there is no `prompt`) before it becomes `{{ input }}` for
    /// the next step. The filtered value must be valid JSON.
    pub(crate) jq: Option<String>,
    /// Retries this step's action (`input_schema` check, `prompt`/`agent`
    /// call, and `jq`, as one unit) up to `max_attempts` times on failure.
    /// Applies before `on_error`, which only runs once every attempt here
    /// has failed.
    pub(crate) retry: Option<RetryDefinition>,
    /// A per-attempt time limit, in seconds, on this step's action. A timed
    /// out attempt counts as a failure for `retry`, the same as any other
    /// error.
    pub(crate) timeout: Option<u64>,
    /// Runs in place of failing the workflow when this step's action (after
    /// every `retry` attempt, if any) still fails. Its steps receive
    /// `{"error": "<the failure message>", "input": <this step's input>}` as
    /// their `{{ input }}`; shape that with a leading `jq` step if the
    /// workflow needs something else.
    pub(crate) on_error: Option<OnErrorDefinition>,
    /// Turns this step into a branch router: evaluates `cases` in order and
    /// runs the first one whose `when` is truthy (or `else`, if none match).
    /// Mutually exclusive with every other field except `id` (including
    /// `parallel`).
    pub(crate) switch: Option<SwitchDefinition>,
    /// Turns this step into a fan-out/fan-in: runs every branch concurrently
    /// against the same input and joins their outputs. Mutually exclusive
    /// with every other field except `id` (including `switch`).
    pub(crate) parallel: Option<ParallelDefinition>,
    /// Turns this step into a conditional loop: re-runs `steps` while/until a
    /// jq condition holds, threading each iteration's output into the next
    /// iteration's `{{ input }}`. Mutually exclusive with every other field
    /// except `id` (including `switch`/`parallel`/`for_each`).
    pub(crate) r#loop: Option<LoopDefinition>,
    /// Turns this step into an array map: runs `steps` once per element of a
    /// jq-selected array, collecting the results (in array order) into a
    /// JSON array. Mutually exclusive with every other field except `id`
    /// (including `switch`/`parallel`/`loop`).
    pub(crate) for_each: Option<ForEachDefinition>,
    /// Ends the workflow successfully right after this step runs (after its
    /// own `prompt`/`agent`/`jq` action, if any), using this step's output as
    /// the workflow's final result; no further steps run. Rejected inside a
    /// `parallel` branch, where concurrently running sibling branches make
    /// "stop the workflow" ambiguous. Mutually exclusive with `break`.
    pub(crate) stop: Option<bool>,
    /// Exits the nearest enclosing `loop`/`for_each` body right after this
    /// step runs, using this step's output as that iteration's result (the
    /// loop then proceeds as if the iteration had finished normally, i.e.
    /// checking `while`/`until` or moving to `join`). Requires an enclosing
    /// `loop`/`for_each` reachable without crossing a `parallel` branch
    /// boundary. Mutually exclusive with `stop`.
    pub(crate) r#break: Option<bool>,
}

/// The step kinds that route to nested `steps` instead of acting directly on
/// their own input, borrowed out of whichever of `StepDefinition::switch`/
/// `parallel`/`loop`/`for_each` is set. See `StepDefinition::router`.
pub(crate) enum Router<'a> {
    Switch(&'a SwitchDefinition),
    Parallel(&'a ParallelDefinition),
    Loop(&'a LoopDefinition),
    ForEach(&'a ForEachDefinition),
}

impl StepDefinition {
    /// Which router kind this step is, if any. `validate::validate_steps`
    /// checks `switch`/`parallel`/`loop`/`for_each` are not set together
    /// before ever calling this (see its `router_count` check), so checking
    /// them in a fixed order here is safe; called on a step that hasn't been
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

#[derive(Debug, Deserialize)]
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
    pub(crate) steps: Vec<StepDefinition>,
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
    pub(crate) else_steps: Option<Vec<StepDefinition>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaseDefinition {
    /// An optional label used only in progress output (like `StepDefinition::id`).
    pub(crate) id: Option<String>,
    /// A jq filter evaluated against the current input; see `StepDefinition::when`.
    pub(crate) when: String,
    pub(crate) steps: Vec<StepDefinition>,
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
    /// A jq filter applied to that id-keyed object, the same way
    /// `StepDefinition::jq` applies to a single step's output. If omitted,
    /// the object itself (serialized as JSON) becomes `{{ input }}` for the
    /// next step.
    pub(crate) join: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchDefinition {
    /// Defaults to `branch-{n}` (1-based), like `StepDefinition::id`. Unlike
    /// a step or case id, this also becomes the branch's key in the joined
    /// JSON object, so it must be unique within its `parallel`.
    pub(crate) id: Option<String>,
    pub(crate) steps: Vec<StepDefinition>,
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
    pub(crate) steps: Vec<StepDefinition>,
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
    pub(crate) steps: Vec<StepDefinition>,
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
