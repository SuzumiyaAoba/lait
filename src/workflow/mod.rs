use std::{fs, path::Path};

use anyhow::{Context, Result, bail};

use crate::{jq, template};

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

/// `FlowStep`'s own YAML field names (see `model.rs`). Used only by
/// `reject_legacy_steps` to recognize a *foreign* field on a `steps[]` entry
/// — most often a `NodeDefinition` field like `prompt`/`jq` left over from
/// the pre-nodes/steps-split schema — and give a migration-shaped error
/// instead of a bare "unknown field" from `#[serde(deny_unknown_fields)]`.
/// Naming `FlowStep`'s allowed fields here, rather than listing
/// `NodeDefinition`'s, means this list only needs updating when `FlowStep`
/// itself grows a field (rare), not whenever a new node-only field is added.
const FLOW_STEP_FIELDS: &[&str] = &[
    "id", "use", "when", "on_error", "switch", "parallel", "loop", "for_each", "stop", "break",
];

/// Detects the pre-nodes/steps-split schema (a `steps[]` entry with a field
/// `FlowStep` doesn't recognize — almost always an action field like
/// `prompt`/`jq` that belongs on a `nodes[]` entry instead) and bails with a
/// migration-shaped message. Recurses into every nested step list (`switch`
/// cases/`else`, `parallel` branches, `loop`/`for_each` bodies, `on_error`)
/// so a file that is new-style at the top level but old-style in a nested
/// block is still caught here. Only called after `WorkflowFile` deserialization
/// has already failed (see `parse_workflow`), so this never costs anything on
/// the success path; a raw `serde_yaml::Value` re-parse of the same text is
/// how it inspects field names deserialization itself already rejected.
fn reject_legacy_steps(raw: &serde_yaml::Value) -> Result<()> {
    let Some(steps) = raw.get("steps").and_then(serde_yaml::Value::as_sequence) else {
        return Ok(());
    };
    reject_legacy_steps_list(steps)
}

fn reject_legacy_steps_list(steps: &[serde_yaml::Value]) -> Result<()> {
    for step in steps {
        let Some(mapping) = step.as_mapping() else {
            continue;
        };
        for key in mapping.keys() {
            let Some(field) = key.as_str() else { continue };
            if !FLOW_STEP_FIELDS.contains(&field) {
                bail!(
                    "this workflow file uses the pre-'nodes:'/'steps:'-split schema: a step has \
                     '{field}' directly. Move step bodies into a top-level 'nodes:' map (keyed by \
                     an id) and reference them from 'steps:' via 'use: <node id>' instead. See \
                     docs/usage/ja/workflow.md."
                );
            }
        }
        if let Some(switch) = step.get("switch") {
            if let Some(cases) = switch.get("cases").and_then(serde_yaml::Value::as_sequence) {
                for case in cases {
                    if let Some(case_steps) =
                        case.get("steps").and_then(serde_yaml::Value::as_sequence)
                    {
                        reject_legacy_steps_list(case_steps)?;
                    }
                }
            }
            if let Some(else_steps) = switch.get("else").and_then(serde_yaml::Value::as_sequence) {
                reject_legacy_steps_list(else_steps)?;
            }
        }
        if let Some(parallel) = step.get("parallel")
            && let Some(branches) = parallel
                .get("branches")
                .and_then(serde_yaml::Value::as_sequence)
        {
            for branch in branches {
                if let Some(branch_steps) =
                    branch.get("steps").and_then(serde_yaml::Value::as_sequence)
                {
                    reject_legacy_steps_list(branch_steps)?;
                }
            }
        }
        for router_key in ["loop", "for_each", "on_error"] {
            if let Some(router) = step.get(router_key)
                && let Some(body) = router.get("steps").and_then(serde_yaml::Value::as_sequence)
            {
                reject_legacy_steps_list(body)?;
            }
        }
    }
    Ok(())
}

fn parse_workflow(contents: &str) -> Result<WorkflowFile> {
    let workflow: WorkflowFile = match serde_yaml::from_str(contents) {
        Ok(workflow) => workflow,
        Err(err) => {
            // A legacy-schema file (an action field like `prompt` directly on
            // a `steps[]` entry) always fails the deserialize above, since
            // `FlowStep` doesn't have those fields. Only re-parse as a raw
            // `Value` here, on the error path, to check for that specific
            // case and give a migration-shaped message; any other parse
            // error is reported as-is.
            if let Ok(raw) = serde_yaml::from_str(contents) {
                reject_legacy_steps(&raw)?;
            }
            return Err(err.into());
        }
    };
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

/// Evaluates a `when`/case-condition jq filter against the current input,
/// using the same JSON-or-string coercion as `{{ input }}` templates
/// (`template::parse_input`) so a `when:` right after a plain-text `prompt:`
/// step doesn't fail just because the input isn't JSON. `steps` is exposed to
/// the filter as `$steps` (see `StepOutputs`).
pub(crate) fn eval_when(filter: &str, current_input: &str, steps: &StepOutputs) -> Result<bool> {
    let value = template::parse_input(current_input);
    let input_json = serde_json::to_string(&value)
        .context("failed to serialize the current input for a 'when' condition")?;
    jq::apply_bool(filter, &input_json, steps).context("failed to evaluate 'when' condition")
}
