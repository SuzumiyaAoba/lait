use std::{fs, path::Path};

use anyhow::{Context, Result, bail};

use crate::jq;

#[cfg(test)]
use crate::template;

mod model;
mod validate;

pub(crate) use model::*;

#[cfg(test)]
mod tests;

pub(crate) fn load_workflow(path: &Path) -> Result<WorkflowFile> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read workflow file '{}'", path.display()))?;
    parse_workflow(&contents)
        .with_context(|| format!("failed to parse workflow file '{}'", path.display()))
}

fn parse_workflow(contents: &str) -> Result<WorkflowFile> {
    let workflow: WorkflowFile = serde_yaml::from_str(contents)?;
    if workflow.steps.is_empty() {
        bail!("workflow must contain at least one step");
    }
    validate::validate_workflow_defaults(&workflow.default)?;
    for (node_id, node) in &workflow.nodes {
        validate::validate_node(node, node_id)?;
    }
    validate::validate_steps(
        &workflow.steps,
        &workflow.nodes,
        validate::FlowContext::TOP_LEVEL,
    )?;
    Ok(workflow)
}

/// Named step outputs recorded by `id` while a workflow runs, exposed to
/// prompts as `{{ steps.<id> }}` and to jq filters (`when`/`jq`/`switch`
/// cases/`loop` conditions/`for_each.items`/every `join`) as the `$steps`
/// global variable. Only steps with an explicit `id` are recorded — the
/// auto-generated `step-N` label used in progress output is not a stable name
/// to reference.
pub(crate) type StepOutputs = jq::Steps;

/// Evaluates a `when`/case-condition jq filter against the current input on a
/// bounded blocking worker. Input coercion/serialization is performed by the
/// worker as well, so a very large plain-text input cannot block a Tokio
/// executor thread before jq starts evaluating it. `steps` is exposed to the
/// filter as `$steps` (see `StepOutputs`).
pub(crate) async fn eval_when_async(
    filter: &str,
    current_input: &str,
    steps: &StepOutputs,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<bool> {
    jq::apply_bool_cancellable_async(filter, current_input, steps, cancellation)
        .await
        .context("failed to evaluate 'when' condition")
}

/// Synchronous helper retained for the pure workflow unit tests. Runtime
/// execution uses [`eval_when_async`] so jq never runs on Tokio's executor.
#[cfg(test)]
pub(crate) fn eval_when(filter: &str, current_input: &str, steps: &StepOutputs) -> Result<bool> {
    let value = template::parse_input(current_input);
    let input_json = serde_json::to_string(&value)
        .context("failed to serialize the current input for a 'when' condition")?;
    jq::apply_bool(filter, &input_json, steps).context("failed to evaluate 'when' condition")
}
