//! Flow/step/router types: what a `steps:` list is actually made of once
//! compiled (see [`super::compile`]), plus [`FlowStep`]'s own read-only
//! accessors. Split out of `workflow/model.rs` — see that module's former
//! doc comment, now on [`super`].

use std::sync::Arc;

use serde::Deserialize;

use super::nodes::NodeDefinition;

/// A control-flow reference site: one position in a `steps:` list. Carries
/// no action of its own — `use` points at a `NodeDefinition` in the
/// workflow's `nodes:` map, or one of `switch`/`parallel`/`loop`/`for_each`
/// routes to nested `steps` instead.
/// A step whose shape and nesting have passed workflow validation.
/// Construction is private; execution and presentation never receive wire shapes.
#[derive(Debug)]
pub(crate) struct FlowStep {
    pub(super) id: Option<String>,
    pub(super) action: StepAction,
}

/// Not visible outside `model` (used only by [`FlowStep`]'s own accessors
/// here and by [`super::compile::FlowStep::compile`], which constructs it).
#[derive(Debug)]
pub(super) enum StepAction {
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
pub(in crate::workflow) struct RawLoopDefinition<S> {
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
