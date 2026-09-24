//! Workflow file loading, validation, and the registry `lait run`/`lait
//! lint` resolve a `FILE` argument or a `workflows:` registry name against
//! (see [`resolve_run_target`]). The document/step types live in `model.rs`
//! (re-exported here via `pub(crate) use model::*`), YAML-to-model parsing
//! and load-time validation in `parse.rs`, `inputs:` binding in `inputs.rs`,
//! and execution in `exec`; this module is the entry point that ties file
//! resolution and parsing together before handing off to
//! `exec::run_document`. [`WorkflowRegistry`] caches a loaded, validated
//! `WorkflowFile` by path so a `workflow:` step inside a `for_each`/`while`/
//! `until` body does not re-read and re-parse the same sub-workflow file on
//! every iteration — the same `AsyncCache`-backed pattern
//! `subagent::AgentRegistry` uses for its own by-path cache.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};

use crate::{async_cache::AsyncCache, async_io, config::ConfigFile, registry, report};

mod ask;
pub(crate) mod dryrun;
pub(crate) mod exec;
pub(crate) mod graph;
pub(crate) mod inputs;
pub(crate) mod model;
pub(crate) mod parse;
pub(crate) mod scope;

pub(crate) use model::*;
pub(crate) use scope::WorkflowScope;

/// Named step outputs recorded by `id` while a workflow runs, exposed to
/// templates as `{{ steps.<id> }}` and to jq as the `$steps` global. Only
/// steps with an explicit `id` are recorded — the auto-generated `step-N`
/// label used in progress output is not a stable name to reference. A
/// copy-on-write `Arc` wrapper (see `jq::Steps`), so handing a snapshot to a
/// jq worker, a `parallel` branch, or a checkpoint write is a refcount bump
/// rather than a deep copy.
pub(crate) type StepOutputs = crate::jq::Steps;

#[cfg(test)]
mod tests;

/// Resolves `lait run`'s `FILE` argument: `argument` itself when it exists as
/// a file, else a `workflows:` registry entry of that name (see
/// `config::WorkflowMap`). A registry entry's path is already absolute by
/// this point — resolved once at config-load time against the directory of
/// whichever config file (project or global) defined it, see
/// `config::load_config`'s `parse_config_file` — so it keeps working from
/// any subdirectory the same way `lait.config.yml` itself is found by
/// walking upward (see `config::find_config_upward`). When `argument` is
/// *both* an existing file and a registered name, the file wins — noted to
/// stderr so the shadowing isn't silent.
pub(crate) fn resolve_run_target(argument: &Path, file_config: &ConfigFile) -> PathBuf {
    if argument.is_file() {
        if let Some(name) = argument.to_str()
            && file_config.workflows.contains_key(name)
        {
            report::note(format_args!(
                "'{name}' exists as a file and is also a 'workflows:' entry; running the file"
            ));
        }
        return argument.to_path_buf();
    }
    let Some(name) = argument.to_str() else {
        return argument.to_path_buf();
    };
    match file_config.workflows.get(name) {
        // The registry also covers names `lait deps` materialized under
        // `.lait/deps/` (merged in by `config::load`), so the note names
        // the registry rather than the specific file it came from.
        Some(resolved) => {
            report::note(format_args!(
                "resolved '{name}' to '{}' via 'workflows:'",
                resolved.display(),
            ));
            resolved.clone()
        }
        None => argument.to_path_buf(),
    }
}

/// Runs `lait workflow list`: prints every configured `workflows:` entry's
/// name, path, and (when the file loads cleanly) its own `description:`. A
/// registry entry whose file is missing or fails to parse is still listed
/// (with a note) rather than aborting the whole command — `lait lint` is
/// where a hard failure on a bad entry belongs.
pub(crate) fn list(file_config: &ConfigFile) -> Result<()> {
    registry::list_path_registry("workflows", &file_config.workflows, |_, path| {
        (
            path.to_owned(),
            load_workflow(path).map(|workflow| workflow.description),
        )
    })
}

pub(crate) fn load_workflow(path: &Path) -> Result<WorkflowFile> {
    let contents = async_io::read_to_string_sync(path)
        .with_context(|| format!("failed to read workflow file '{}'", path.display()))?;
    parse::parse_workflow(&contents, base_dir_of(path))
        .with_context(|| format!("failed to parse workflow file '{}'", path.display()))
}

/// Loads and validates a workflow without blocking the async executor. The
/// worker owns the bounded read and parsing work, and observes cancellation
/// while waiting for a FIFO writer or reading the source.
pub(crate) async fn load_workflow_cancellable(
    path: &Path,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<WorkflowFile> {
    let path = path.to_owned();
    async_io::run_blocking(
        move |cancelled| {
            let contents = async_io::read_to_string_wait_for_fifo_writer(
                &path,
                cancelled,
                async_io::MAX_READ_BYTES,
            )
            .with_context(|| format!("failed to read workflow file '{}'", path.display()))?;
            parse::parse_workflow(&contents, base_dir_of(&path))
                .with_context(|| format!("failed to parse workflow file '{}'", path.display()))
        },
        cancellation,
    )
    .await
}

/// Caches parsed sub-workflow files across a run, keyed by their canonical
/// path (see `WorkflowScope::check_nested_path`, which canonicalizes and
/// cycle-checks before this is ever consulted), matching `AgentRegistry`/
/// `SkillCache`'s own caches — see `AppServices`. Without this, a
/// `for_each`/`while`/`until` body with a `workflow:` step would re-read and
/// re-parse the same YAML on every iteration; agents and skills already
/// avoid exactly this.
pub(crate) struct WorkflowRegistry {
    loaded: AsyncCache<PathBuf, WorkflowFile>,
}

impl WorkflowRegistry {
    pub(crate) fn new() -> Self {
        Self {
            loaded: AsyncCache::new(),
        }
    }

    /// Returns the workflow file at `path`, loading and parsing it (see
    /// [`load_workflow_cancellable`]) on first use, then caching the result
    /// for the registry's lifetime.
    pub(crate) async fn load_path_cancellable(
        &self,
        path: &Path,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Arc<WorkflowFile>> {
        let load_cancellation = cancellation.clone();
        self.loaded
            .get_or_try_init(
                path.to_path_buf(),
                cancellation,
                || async move {
                    load_workflow_cancellable(path, load_cancellation)
                        .await
                        .map(Arc::new)
                },
                "nested workflow load was cancelled",
            )
            .await
    }
}

/// The directory relative definition references in a workflow file are
/// resolved against.
fn base_dir_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Parses workflow YAML as if it lived in the current directory: used by
/// `deps::ops` to validate a fetched workflow file's bytes before the
/// dependency is registered (the same parse a later `lait run` would do,
/// moved to the fetch boundary), and by tests and schema cross-checks.
pub(crate) fn parse_workflow(contents: &str) -> Result<WorkflowFile> {
    parse::parse_workflow(contents, Path::new("."))
}
