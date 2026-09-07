//! The scope in effect for one workflow file's steps: its `default:` block,
//! model aliases and JSON schemas, plus the cycle/depth
//! bookkeeping for `workflow:` nesting. Read by every
//! `resolve_step_settings`/`execute_step` call in `super::exec`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};

use crate::{async_io, config::ModelMap, nesting, schema};

use super::model::{WorkflowDefaults, WorkflowFile};

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
    /// Directory relative paths in this scope's workflow file (currently
    /// only `node.workflow`) are resolved against.
    pub(crate) base_dir: PathBuf,
    pub(crate) active_paths: Vec<PathBuf>,
}

impl WorkflowScope {
    /// Moves defaults and model/schema aliases into the execution scope.
    /// Steps already own shared references to their validated node definitions,
    /// so node lookup does not depend on this scope's identity or mutability.
    pub(crate) async fn top_level(
        wf: &mut WorkflowFile,
        file_path: &Path,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Self> {
        let canonical = async_io::canonicalize(file_path, cancellation)
            .await
            .with_context(|| {
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
            base_dir,
            active_paths: vec![canonical],
        })
    }

    /// Builds a nested file scope, merging its model/schema/default overrides
    /// and checking canonical file paths for cycles and excessive nesting.
    /// Node references remain bound to the file where the step was parsed.
    /// Empty model/schema layers share the parent's Arc without cloning entries.
    pub(crate) async fn resolve_nested_path(
        &self,
        relative_path: &Path,
        label: &str,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<PathBuf> {
        let resolved_path = self.base_dir.join(relative_path);
        let canonical = async_io::canonicalize(&resolved_path, cancellation)
            .await
            .with_context(|| {
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

        Ok(canonical)
    }

    /// Builds a child scope from a path already checked for cycles and depth.
    pub(crate) fn nested(&self, canonical: PathBuf, sub_wf: &mut WorkflowFile) -> Self {
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

        Self {
            defaults: WorkflowDefaults::fold(&[
                std::mem::take(&mut sub_wf.default),
                self.defaults.clone(),
            ]),
            models,
            json_schemas,
            base_dir,
            active_paths,
        }
    }
}
