use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{config::ConfigFile, jq};

#[cfg(test)]
use crate::template;

mod ask;
pub(crate) mod dryrun;
pub(crate) mod exec;
pub(crate) mod graph;
mod model;
pub(crate) mod scope;
mod validate;

pub(crate) use model::*;
pub(crate) use scope::WorkflowScope;

#[cfg(test)]
mod tests;

/// Resolves `lait run`'s `FILE` argument: `argument` itself when it exists as
/// a file, else a `workflows:` registry entry of that name (see
/// `config::WorkflowMap`), resolved against `config_dir` — the directory of
/// the `lait.config.yml` that defined the registry, not the current working
/// directory, so a registry entry keeps working from any subdirectory the
/// same way `lait.config.yml` itself is found by walking upward (see
/// `config::find_config_upward`). `config_dir` is `None` when no config file
/// was read at all (`--no-config`, or `Search` finding nothing), in which
/// case a registry path (if any; `workflows:` is then always empty anyway)
/// falls back to being read relative to the current directory. When
/// `argument` is *both* an existing file and a registered name, the file
/// wins — noted to stderr so the shadowing isn't silent.
pub(crate) fn resolve_run_target(
    argument: &Path,
    file_config: &ConfigFile,
    config_dir: Option<&Path>,
) -> PathBuf {
    if argument.is_file() {
        if let Some(name) = argument.to_str()
            && file_config.workflows.contains_key(name)
        {
            eprintln!(
                "note: '{name}' exists as a file and is also a 'workflows:' entry; running the file"
            );
        }
        return argument.to_path_buf();
    }
    let Some(name) = argument.to_str() else {
        return argument.to_path_buf();
    };
    match file_config.workflows.get(name) {
        Some(registered_path) => {
            let resolved = crate::config::resolve_registry_path(registered_path, config_dir);
            eprintln!(
                "note: resolved '{name}' to '{}' via 'workflows:' in {}",
                resolved.display(),
                crate::config::CONFIG_FILE_NAME
            );
            resolved
        }
        None => argument.to_path_buf(),
    }
}

/// Runs `lait workflow list`: prints every configured `workflows:` entry's
/// name, path, and (when the file loads cleanly) its own `description:`.
/// `config_dir` resolves each entry's path the same way
/// `resolve_run_target`/`lint::check_workflows_registry` do — see
/// `config::resolve_registry_path`. A registry entry whose file is missing or
/// fails to parse is still listed (with a note) rather than aborting the
/// whole command — `lait lint` is where a hard failure on a bad entry
/// belongs.
pub(crate) fn list(file_config: &ConfigFile, config_dir: Option<&Path>) -> Result<()> {
    if file_config.workflows.is_empty() {
        println!(
            "no workflows defined in {}; add a 'workflows:' entry to define one",
            crate::config::CONFIG_FILE_NAME
        );
        return Ok(());
    }
    let mut names: Vec<&String> = file_config.workflows.keys().collect();
    names.sort_unstable();
    for name in names {
        let raw_path = &file_config.workflows[name];
        let path = crate::config::resolve_registry_path(raw_path, config_dir);
        let loaded = load_workflow(&path).map(|wf| wf.description);
        crate::config::print_registry_entry(name, &path, loaded);
    }
    Ok(())
}

pub(crate) fn load_workflow(path: &Path) -> Result<WorkflowFile> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read workflow file '{}'", path.display()))?;
    parse_workflow(&contents)
        .with_context(|| format!("failed to parse workflow file '{}'", path.display()))
}

fn parse_workflow(contents: &str) -> Result<WorkflowFile> {
    let workflow: WorkflowFile = serde_yaml::from_str(contents).map_err(|error| {
        // A node with no `type:` at all — the pre-version schema's shape —
        // fails here with a "missing field `type`" message from the
        // now-tagged `NodeDefinition` enum. That message alone doesn't say
        // *why*, so point the author at the fix instead of leaving them to
        // find B-1's changelog entry.
        if error.to_string().contains("missing field `type`") {
            anyhow!(
                "{error}\n\nevery entry under 'nodes:' now requires a 'type:' \
                 (prompt/agent/workflow/command/transform/ask); see docs/usage/ja/workflow.md"
            )
        } else {
            error.into()
        }
    })?;
    if let Some(version) = workflow.version
        && version != CURRENT_WORKFLOW_VERSION
    {
        bail!(
            "unsupported workflow schema 'version: {version}'; this build of lait supports \
             version {CURRENT_WORKFLOW_VERSION} (omit 'version:' to use the latest one this \
             build supports)"
        );
    }
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

/// Builds the `vars` object a `lait run --var KEY=VALUE` invocation exposes
/// to step templates as `{{ vars.<key> }}` and to jq filters as
/// `$vars.<key>` (see `engine::AppContext::vars`). Unlike a named prompt's
/// `--var` (`prompt::build_vars`, always a string), each VALUE is parsed as
/// JSON when possible — `--var items='["a","b"]'` becomes a structured
/// array/object rather than its literal text — falling back to a plain JSON
/// string otherwise, so `--var lang=ja` still renders as `ja`. A later
/// `--var` for the same key wins.
pub(crate) fn build_vars(
    cli_vars: &[String],
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut vars = serde_json::Map::new();
    for raw in cli_vars {
        let (key, value) = crate::prompt::parse_var(raw)?;
        vars.insert(key, crate::template::parse_input(&value));
    }
    Ok(vars)
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
/// filter as `$steps` (see `StepOutputs`), `vars` as `$vars` (see
/// `engine::AppContext::vars`).
pub(crate) async fn eval_when_async(
    filter: &str,
    current_input: &str,
    steps: &StepOutputs,
    vars: &StepOutputs,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<bool> {
    jq::apply_bool_cancellable_async(filter, current_input, steps, vars, cancellation)
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
    jq::apply_bool(filter, &input_json, steps, &StepOutputs::new())
        .context("failed to evaluate 'when' condition")
}
