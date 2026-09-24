//! The scope in effect while one workflow file's steps run: its merged
//! `default:` block and model aliases, its resolved `inputs`, and the
//! cycle/depth bookkeeping for nested `workflow:` steps.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};

use crate::{async_io, config::ModelMap, jq, nesting};

use super::model::{WorkflowDefaults, WorkflowFile};

pub(crate) struct WorkflowScope {
    /// This file's `default:` merged over its callers', field by field.
    pub(crate) defaults: WorkflowDefaults,
    /// This file's `models:` merged over its callers'. Shared via `Arc`
    /// when a child defines none of its own.
    pub(crate) models: Arc<ModelMap>,
    /// This file's resolved `inputs`, exposed as `{{ inputs.* }}`/`$inputs`.
    /// Copy-on-write (see `jq::Steps`), so the per-step `jq::Globals` built
    /// from it is a refcount bump rather than a deep copy.
    pub(crate) inputs: jq::Steps,
    /// Canonical paths of every workflow file currently running in this
    /// chain, to reject a `workflow:` cycle and cap nesting depth.
    pub(crate) active_paths: Vec<PathBuf>,
}

impl WorkflowScope {
    /// The scope of the file passed to `lait run`/`lait test`/`lait eval`.
    pub(crate) async fn top_level(
        wf: &WorkflowFile,
        file_path: &Path,
        inputs: serde_json::Map<String, serde_json::Value>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        let canonical = async_io::canonicalize(file_path, cancellation)
            .await
            .with_context(|| {
                format!(
                    "failed to resolve workflow file path '{}'",
                    file_path.display()
                )
            })?;
        Ok(Self {
            defaults: wf.defaults.clone(),
            models: Arc::new(wf.models.clone()),
            inputs: inputs.into(),
            active_paths: vec![canonical],
        })
    }

    /// Canonicalizes a child workflow path and rejects a cycle or excessive
    /// nesting before the child file is opened.
    pub(crate) async fn check_nested_path(
        &self,
        path: &Path,
        label: &str,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<PathBuf> {
        let canonical = async_io::canonicalize(path, cancellation)
            .await
            .with_context(|| {
                format!(
                    "step '{label}': failed to resolve workflow file path '{}'",
                    path.display()
                )
            })?;
        if let Err(error) = nesting::check_workflow_nesting(&self.active_paths, &canonical) {
            match error {
                nesting::NestingDepthError::Cycle => bail!(
                    "step '{label}': 'workflow: {}' would create a cycle ('{}' is already running)",
                    path.display(),
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

    /// The scope of a child workflow: its own `default:`/`models:` win over
    /// this scope's, and it sees only its own resolved `inputs`.
    pub(crate) fn nested(
        &self,
        canonical: PathBuf,
        child: &WorkflowFile,
        inputs: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let models = if child.models.is_empty() {
            Arc::clone(&self.models)
        } else {
            let mut merged = child.models.clone();
            for (name, definitions) in self.models.iter() {
                merged
                    .entry(name.clone())
                    .or_insert_with(|| definitions.clone());
            }
            Arc::new(merged)
        };
        let mut active_paths = self.active_paths.clone();
        active_paths.push(canonical);
        Self {
            defaults: WorkflowDefaults::fold(&[child.defaults.clone(), self.defaults.clone()]),
            models,
            inputs: inputs.into(),
            active_paths,
        }
    }
}
