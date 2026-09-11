//! Validated workflow step/router/node types — the shapes `exec::run_steps`
//! actually interprets. These are reached only through `raw::FlowStep` +
//! `validate::validate_steps` (see `super`'s module doc for the
//! deserialize → validate → model pipeline); nothing outside that pipeline
//! constructs a `NodeDefinition` directly, which is what lets `exec` assume
//! every structural rule `validate` checks (router/action-field exclusivity,
//! duplicate labels, ...) already holds by the time it sees one.
//!
//! Split into four submodules, each re-exported here so every name below
//! still resolves as `model::<Name>` (and, via `super`'s own `pub(crate) use
//! model::*`, as `workflow::<Name>`) exactly as when this was one file:
//! [`document`] (the document/`default:` shape), [`nodes`] (`NodeDefinition`
//! and its variants), [`flow`] (`FlowStep` and the router/retry/loop
//! definitions), and [`compile`] (the only real logic here — binding
//! `use:` references and compiling `raw::FlowStep` into the private
//! `flow::StepAction` variants). `NodeMap`/`WorkflowFile`/`RawWorkflowFile`
//! stay here rather than in any one submodule since each spans at least two
//! of them (`NodeMap` keys on `nodes::NodeDefinition`;
//! `WorkflowFile`/`RawWorkflowFile` instantiate `document::WorkflowDocument`
//! with a `flow`/`raw` step type).

mod compile;
mod document;
mod flow;
mod nodes;

pub(crate) use document::{CURRENT_WORKFLOW_VERSION, WorkflowDefaults, WorkflowDocument};
pub(crate) use flow::{
    Control, FlowStep, ForEachDefinition, LoopCondition, LoopDefinition, OnErrorDefinition,
    ParallelDefinition, RetryDefinition, Router, SwitchDefinition,
};
// `pub(in crate::workflow)`, not `pub(crate)`: `raw.rs` (a sibling of this
// module under `workflow`) is the only outside user, via
// `super::model::RawLoopDefinition` — see `flow::RawLoopDefinition`'s own
// doc comment for why it isn't crate-wide.
pub(in crate::workflow) use flow::RawLoopDefinition;
pub(crate) use nodes::{AgentNode, AskNode, CommandNode, NodeDefinition, PromptNode, WorkflowNode};
// `NodeKind` itself is only ever named directly by the pure workflow unit
// tests (every production call site gets it through `NodeDefinition::kind()`
// without needing to spell the type) — gated the same way `workflow/mod.rs`
// gates its own test-only `use crate::template`, so a non-test build
// doesn't flag this re-export as unused.
#[cfg(test)]
pub(crate) use nodes::NodeKind;

/// The workflow-file-scoped map of reusable action definitions, keyed by the
/// name used in `steps[].use`. Unlike `models`/`json_schemas`, this is never
/// merged into a nested `workflow:` step's sub-workflow scope — each file's
/// `use:` resolves only against its own `nodes:` during validation. Compiled
/// call steps retain an Arc to that definition for their entire lifetime.
pub(crate) type NodeMap = std::collections::BTreeMap<String, std::sync::Arc<NodeDefinition>>;

pub(crate) type WorkflowFile = WorkflowDocument<FlowStep>;
pub(in crate::workflow) type RawWorkflowFile = WorkflowDocument<super::raw::FlowStep>;
