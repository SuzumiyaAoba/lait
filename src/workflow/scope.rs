//! The scope in effect for one workflow file's steps: its `default:` block,
//! model aliases, JSON schemas, and `nodes:` map, plus the cycle/depth
//! bookkeeping for `workflow:` nesting. Read by every
//! `resolve_step_settings`/`execute_step` call in `super::exec`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{config::ModelMap, nesting, schema};

use super::model::{NodeMap, WorkflowDefaults, WorkflowFile};

/// The default model/reasoning-effort, model aliases, and JSON schema
/// definitions currently in effect, plus enough bookkeeping to run a nested
/// `workflow:` step safely. Every `resolve_step_settings`/`execute_step` call
/// reads through this instead of a `&workflow::WorkflowFile` directly, so a
/// `workflow:` step's sub-workflow can see its own `default:`/`models:`/
/// `json_schemas:` first, falling back to its caller's (`nested` builds
/// that merge). `active_paths` records every workflow file currently
/// executing (canonicalized), to reject a `workflow:` cycle and to cap
/// nesting depth at `MAX_WORKFLOW_DEPTH`.
pub(crate) struct WorkflowScope {
    /// The `default:` block in effect for this scope's steps. Merged across
    /// `workflow:` nesting field by field — a sub-workflow's own entry wins,
    /// falling back to its caller's when unset (see
    /// `workflow::WorkflowDefaults::or_fallback`); only `retry` falls back as
    /// a whole struct rather than field-by-field.
    pub(crate) defaults: WorkflowDefaults,
    pub(crate) models: ModelMap,
    pub(crate) json_schemas: schema::JsonSchemaMap,
    /// This scope's own `nodes:` map, resolved by every `steps[].use` in this
    /// file. Unlike `models`/`json_schemas`, a `workflow:` node's sub-scope
    /// does *not* fall back to this scope's `nodes` for entries it lacks —
    /// each workflow file's `use:` sites only ever see that file's own
    /// `nodes:` (see `WorkflowScope::nested`).
    pub(crate) nodes: NodeMap,
    /// Directory relative paths in this scope's workflow file (currently
    /// only `node.workflow`) are resolved against.
    pub(crate) base_dir: PathBuf,
    pub(crate) active_paths: Vec<PathBuf>,
}

impl WorkflowScope {
    /// The scope for the workflow file passed on the command line. Takes
    /// `wf.default`/`wf.nodes` by move (via `mem::take`) rather than cloning
    /// them: neither is ever read again after this call, only `wf.steps`
    /// (see `run_workflow`).
    pub(crate) fn top_level(wf: &mut WorkflowFile, file_path: &Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(file_path).with_context(|| {
            format!(
                "failed to resolve workflow file path '{}'",
                file_path.display()
            )
        })?;
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            defaults: std::mem::take(&mut wf.default),
            models: wf.models.clone(),
            json_schemas: wf.json_schemas.clone(),
            nodes: std::mem::take(&mut wf.nodes),
            base_dir,
            active_paths: vec![canonical],
        })
    }

    /// The scope for a `workflow:` node's sub-workflow: resolves
    /// `relative_path` (as given in the node) against this scope's
    /// `base_dir`, merges `sub_wf`'s `default`/`models`/`json_schemas` over
    /// this scope's (the sub-workflow's own entries win; an entry it doesn't
    /// define falls back to this scope's), takes `sub_wf`'s `default`/`nodes:`
    /// by move (`nodes` gets no fallback — see `WorkflowScope::nodes` — and
    /// neither is ever read again after this call, only `sub_wf.steps`), and
    /// extends the cycle/depth bookkeeping. Fails if `relative_path`
    /// resolves to a workflow file already executing (a cycle) or nesting
    /// has reached `MAX_WORKFLOW_DEPTH`.
    pub(crate) fn nested(
        &self,
        relative_path: &Path,
        sub_wf: &mut WorkflowFile,
        label: &str,
    ) -> Result<Self> {
        let resolved_path = self.base_dir.join(relative_path);
        let canonical = std::fs::canonicalize(&resolved_path).with_context(|| {
            format!(
                "step '{label}': failed to resolve workflow file path '{}'",
                resolved_path.display()
            )
        })?;
        if let Err(error) = nesting::check_workflow_nesting(&self.active_paths, &canonical) {
            match error {
                nesting::NestingDepthError::Cycle => bail!(
                    "step '{label}': 'workflow: {}' would create a cycle ('{}' is already running)",
                    relative_path.display(),
                    canonical.display()
                ),
                nesting::NestingDepthError::TooDeep => bail!(
                    "step '{label}': 'workflow:' nesting exceeded the maximum depth of {}",
                    nesting::MAX_WORKFLOW_DEPTH
                ),
            }
        }

        let mut models = sub_wf.models.clone();
        for (name, definitions) in &self.models {
            models
                .entry(name.clone())
                .or_insert_with(|| definitions.clone());
        }
        let mut json_schemas = sub_wf.json_schemas.clone();
        for (name, entry) in &self.json_schemas {
            json_schemas
                .entry(name.clone())
                .or_insert_with(|| entry.clone());
        }
        let mut active_paths = self.active_paths.clone();
        active_paths.push(canonical.clone());
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(Self {
            defaults: std::mem::take(&mut sub_wf.default).or_fallback(&self.defaults),
            models,
            json_schemas,
            nodes: std::mem::take(&mut sub_wf.nodes),
            base_dir,
            active_paths,
        })
    }
}
