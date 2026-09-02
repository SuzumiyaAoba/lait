//! The scope in effect for one workflow file's steps: its `default:` block,
//! model aliases, JSON schemas, and `nodes:` map, plus the cycle/depth
//! bookkeeping for `workflow:` nesting. Read by every
//! `resolve_step_settings`/`execute_step` call in `super::exec`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

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
    /// `workflow::WorkflowDefaults::fold`); only `retry` falls back as
    /// a whole struct rather than field-by-field.
    pub(crate) defaults: WorkflowDefaults,
    /// `Arc`-wrapped so a nested scope that defines no local `models:`/
    /// `json_schemas:` of its own (the common case) can share its parent's
    /// map with a cheap `Arc::clone` instead of cloning every entry again —
    /// see `nested`. Only a scope that actually overrides an alias/schema
    /// pays for a fresh merged map.
    pub(crate) models: Arc<ModelMap>,
    pub(crate) json_schemas: Arc<schema::JsonSchemaMap>,
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
    /// every field but `wf.steps` by move (via `mem::take`) rather than
    /// cloning it: none of them are ever read again after this call, only
    /// `wf.steps` (see `run_workflow`).
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
            models: Arc::new(std::mem::take(&mut wf.models)),
            json_schemas: Arc::new(std::mem::take(&mut wf.json_schemas)),
            nodes: std::mem::take(&mut wf.nodes),
            base_dir,
            active_paths: vec![canonical],
        })
    }

    /// The scope for a `workflow:` node's sub-workflow: resolves
    /// `relative_path` (as given in the node) against this scope's
    /// `base_dir`, merges `sub_wf`'s `default`/`models`/`json_schemas` over
    /// this scope's (the sub-workflow's own entries win; an entry it doesn't
    /// define falls back to this scope's), takes every `sub_wf` field but
    /// `steps` by move (none are ever read again after this call, only
    /// `sub_wf.steps` — `nodes` gets no fallback, see `WorkflowScope::nodes`),
    /// and extends the cycle/depth bookkeeping. Fails if `relative_path`
    /// resolves to a workflow file already executing (a cycle) or nesting
    /// has reached `MAX_WORKFLOW_DEPTH`.
    ///
    /// When `sub_wf` defines no local `models:`/`json_schemas:` of its own
    /// (the common case for a deeply nested `workflow:` chain), this scope's
    /// own `Arc<ModelMap>`/`Arc<JsonSchemaMap>` are shared as-is rather than
    /// rebuilt — avoiding the O(depth × map size) clone cost a full
    /// re-merge at every nesting level would otherwise add.
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

        let models = if sub_wf.models.is_empty() {
            Arc::clone(&self.models)
        } else {
            let mut merged = std::mem::take(&mut sub_wf.models);
            for (name, definitions) in self.models.iter() {
                merged
                    .entry(name.clone())
                    .or_insert_with(|| definitions.clone());
            }
            Arc::new(merged)
        };
        let json_schemas = if sub_wf.json_schemas.is_empty() {
            Arc::clone(&self.json_schemas)
        } else {
            let mut merged = std::mem::take(&mut sub_wf.json_schemas);
            for (name, entry) in self.json_schemas.iter() {
                merged.entry(name.clone()).or_insert_with(|| entry.clone());
            }
            Arc::new(merged)
        };
        let mut active_paths = self.active_paths.clone();
        active_paths.push(canonical.clone());
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(Self {
            defaults: WorkflowDefaults::fold(&[
                std::mem::take(&mut sub_wf.default),
                self.defaults.clone(),
            ]),
            models,
            json_schemas,
            nodes: std::mem::take(&mut sub_wf.nodes),
            base_dir,
            active_paths,
        })
    }
}
