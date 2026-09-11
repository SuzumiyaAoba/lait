//! Raw-to-executable compilation: [`FlowStep::compile`]/[`compile_steps`]
//! bind `use:` references to their `NodeDefinition` and build the private
//! [`super::flow::StepAction`] variants, and
//! [`WorkflowDocument::validate`](document::WorkflowDocument::validate) is
//! the only entry point that runs full structural validation before either.
//! The one piece of real logic in `model` — split out of `workflow/model.rs`,
//! see that module's former doc comment, now on [`super`].

use anyhow::{Context, Result};

use super::{
    CURRENT_WORKFLOW_VERSION, NodeMap, WorkflowFile,
    document::WorkflowDocument,
    flow::{
        BranchDefinition, CaseDefinition, Control, FlowStep, ForEachDefinition, LoopCondition,
        LoopDefinition, OnErrorDefinition, ParallelDefinition, StepAction, SwitchDefinition,
    },
};

impl FlowStep {
    pub(super) fn compile(raw: super::super::raw::FlowStep, nodes: &NodeMap) -> Result<Self> {
        let control = if raw.stop == Some(true) {
            Control::Stop
        } else if raw.r#break == Some(true) {
            Control::Break
        } else {
            Control::Continue
        };
        let action = if let Some(node) = raw.r#use {
            StepAction::Call {
                definition: std::sync::Arc::clone(
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

pub(super) fn compile_steps(
    steps: Vec<super::super::raw::FlowStep>,
    nodes: &NodeMap,
) -> Result<Vec<FlowStep>> {
    steps
        .into_iter()
        .map(|step| FlowStep::compile(step, nodes))
        .collect()
}

impl WorkflowDocument<super::super::raw::FlowStep> {
    /// The only raw-to-executable conversion validates the entire document
    /// before binding node references and compiling private action variants.
    pub(in crate::workflow) fn validate(self) -> Result<WorkflowFile> {
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
        super::super::validate::validate_workflow_defaults(&self.default)?;
        for (node_id, node) in &self.nodes {
            super::super::validate::validate_node(node, node_id)?;
        }
        super::super::validate::validate_steps(
            &self.steps,
            &self.nodes,
            super::super::validate::FlowContext::TOP_LEVEL,
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
