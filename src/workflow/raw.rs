//! Deserialization-only step shapes. These never enter the interpreter.

use serde::Deserialize;

pub(super) type OnErrorDefinition = super::model::OnErrorDefinition<FlowStep>;
pub(super) type SwitchDefinition = super::model::SwitchDefinition<FlowStep>;
pub(super) type ParallelDefinition = super::model::ParallelDefinition<FlowStep>;
pub(super) type LoopDefinition = super::model::RawLoopDefinition<FlowStep>;
pub(super) type ForEachDefinition = super::model::ForEachDefinition<FlowStep>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FlowStep {
    /// This site's label, used for progress output and as the key this
    /// site's output is recorded under in `{{ steps.<id> }}`/`$steps`.
    /// Defaults to the referenced node's id (see `FlowStep::label`). Required
    /// to differ from any node id it does not itself reference, since node
    /// ids and site ids share the same `$steps` namespace (see
    /// `validate::validate_steps`).
    pub(super) id: Option<String>,
    /// The id of the node (in the workflow's `nodes:` map) this site runs.
    /// Mutually exclusive with `switch`/`parallel`/`loop`/`for_each`; exactly
    /// one of the five, or none of them together with `stop`/`break`, is
    /// required.
    #[serde(rename = "use")]
    pub(super) r#use: Option<String>,
    /// A jq filter evaluated against the current input (JSON-parsed, falling
    /// back to a JSON string for plain text, like `template::parse_input`).
    /// A falsy result (`false`/`null`) skips this site entirely, passing the
    /// input through unchanged to the next step. Only meaningful together
    /// with `use`.
    pub(super) when: Option<String>,
    /// Runs in place of failing the workflow when this site's node (after
    /// every `retry` attempt, if any) still fails. Only meaningful together
    /// with `use`.
    pub(super) on_error: Option<OnErrorDefinition>,
    /// Turns this site into a branch router: evaluates `cases` in order and
    /// runs the first one whose `when` is truthy (or `else`, if none match).
    /// Mutually exclusive with every other field except `id`.
    pub(super) switch: Option<SwitchDefinition>,
    /// Turns this site into a fan-out/fan-in: runs every branch concurrently
    /// against the same input and joins their outputs. Mutually exclusive
    /// with every other field except `id`.
    pub(super) parallel: Option<ParallelDefinition>,
    /// Turns this site into a conditional loop: re-runs `steps` while/until a
    /// jq condition holds, threading each iteration's output into the next
    /// iteration's `{{ input }}`. Mutually exclusive with every other field
    /// except `id`.
    pub(super) r#loop: Option<LoopDefinition>,
    /// Turns this site into an array map: runs `steps` once per element of a
    /// jq-selected array, collecting the results (in array order) into a
    /// JSON array. Mutually exclusive with every other field except `id`.
    pub(super) for_each: Option<ForEachDefinition>,
    /// Ends the workflow successfully right after this site's node runs
    /// (after its own action, if any), using its output as the workflow's
    /// final result; no further steps run. Rejected inside a `parallel`
    /// branch, where concurrently running sibling branches make "stop the
    /// workflow" ambiguous. Mutually exclusive with `break`. May accompany
    /// `use` (checked after the node runs and its output is recorded), or
    /// stand alone.
    pub(super) stop: Option<bool>,
    /// Exits the nearest enclosing `loop`/`for_each` body right after this
    /// site's node runs, using its output as that iteration's result (the
    /// loop then proceeds as if the iteration had finished normally, i.e.
    /// checking `while`/`until` or moving to `join`). Requires an enclosing
    /// `loop`/`for_each` reachable without crossing a `parallel` branch
    /// boundary. Mutually exclusive with `stop`. May accompany `use`, or
    /// stand alone.
    pub(super) r#break: Option<bool>,
}

impl FlowStep {
    /// This site's label for progress output and `$steps` recording: its own
    /// `id` if set, else the referenced node's id (for a `use` site), else
    /// `None` (a router site with no `id`, whose caller falls back to a
    /// `step-N` counter label).
    pub(super) fn label(&self) -> Option<&str> {
        self.id.as_deref().or(self.r#use.as_deref())
    }

    /// `label()`, falling back to `step-<fallback_n>` when this site has
    /// neither an explicit `id` nor a `use` to name it. Shared by
    /// `run_steps`' progress labels and `validate_steps`' error labels, so
    /// both name a given site the same way.
    pub(super) fn label_or(&self, fallback_n: usize) -> String {
        self.label()
            .map(str::to_string)
            .unwrap_or_else(|| format!("step-{fallback_n}"))
    }
}

/// The step kinds that route to nested `steps` instead of acting directly on
/// their own input, borrowed out of whichever of `FlowStep::switch`/
/// `parallel`/`loop`/`for_each` is set. See `FlowStep::router`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RouterKind {
    Switch,
    Parallel,
    Loop,
    ForEach,
}

pub(super) enum Router<'a> {
    Switch(&'a SwitchDefinition),
    Parallel(&'a ParallelDefinition),
    Loop(&'a LoopDefinition),
    ForEach(&'a ForEachDefinition),
}

impl FlowStep {
    /// Returns the configured router kind, if this step has one.
    ///
    /// This is the single source of truth for router presence. An ambiguous
    /// step (more than one router field set) returns `None`, so callers cannot
    /// accidentally execute whichever router happens to come first. Validation
    /// uses [`Self::router_count`] to report that malformed shape explicitly.
    pub(super) fn router_kind(&self) -> Option<RouterKind> {
        if self.router_count() != 1 {
            return None;
        }
        if self.switch.is_some() {
            return Some(RouterKind::Switch);
        }
        if self.parallel.is_some() {
            return Some(RouterKind::Parallel);
        }
        if self.r#loop.is_some() {
            return Some(RouterKind::Loop);
        }
        self.for_each.as_ref().map(|_| RouterKind::ForEach)
    }

    /// Counts router fields set on this step. A valid step has zero or one;
    /// the count is exposed so validation does not duplicate the field list
    /// that `router_kind()` owns.
    pub(super) fn router_count(&self) -> usize {
        usize::from(self.switch.is_some())
            + usize::from(self.parallel.is_some())
            + usize::from(self.r#loop.is_some())
            + usize::from(self.for_each.is_some())
    }

    /// Which router kind this site is, if exactly one router field is set.
    /// `validate::validate_steps` reports the more useful field-level error
    /// before execution, while this method remains safe for any caller that
    /// receives an unvalidated `FlowStep`.
    /// `validate_steps` and `run_steps` both match on this so a new router
    /// kind requires updating both.
    pub(super) fn router(&self) -> Option<Router<'_>> {
        match self.router_kind()? {
            RouterKind::Switch => self.switch.as_ref().map(Router::Switch),
            RouterKind::Parallel => self.parallel.as_ref().map(Router::Parallel),
            RouterKind::Loop => self.r#loop.as_ref().map(Router::Loop),
            RouterKind::ForEach => self.for_each.as_ref().map(Router::ForEach),
        }
    }
}
