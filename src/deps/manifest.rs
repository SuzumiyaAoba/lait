//! `lait.deps.yml` — the dependency manifest `lait deps` commands manage.
//! It is the hand-editable half of the dependency state (`lait.lock`, see
//! `super::lock`, is the generated half): each entry names a single file in
//! a GitHub repository plus an optional git ref and kind, and is found by
//! walking ancestor directories from the current one — the same rule
//! `config::load`'s `find_config_upward` applies to `lait.config.yml`, so a
//! manifest at the repository root keeps working when `lait` is invoked
//! from a subdirectory.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{async_io, storage};

use super::spec::{self, DepKind, GitHubSpec};

/// The manifest file's own name, searched upward from the current
/// directory like [`crate::config::CONFIG_FILE_NAME`] is.
pub(crate) const MANIFEST_FILE_NAME: &str = "lait.deps.yml";

/// The on-disk format version written by this build. A manifest declaring
/// anything newer is rejected rather than misparsed (the same policy
/// `lock.rs` applies to `lait.lock`).
pub(crate) const MANIFEST_VERSION: u32 = 1;

/// One `deps.<name>` entry in `lait.deps.yml`: what to fetch (`source`,
/// optionally pinned by `ref`) and which registry it joins (`kind`,
/// inferred from the source path's extension when omitted — see
/// [`DepKind::infer`]).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DepRequirement {
    /// Any source spelling `spec::parse` accepts; `deps add` writes the
    /// canonical `github:OWNER/REPO/PATH` form back here.
    pub(crate) source: String,
    /// Branch, tag, or commit to fetch `source` at. `None` tracks the
    /// repository's default branch (resolved via the API at fetch time).
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<DepKind>,
}

impl DepRequirement {
    /// Parses `source` and overlays `ref`/`kind`: the manifest field wins
    /// over an `@REF` embedded in the source string, and an explicit `kind`
    /// beats extension inference. The resolved `(spec, kind)` pair is what
    /// both `deps` commands and the config-registry merge work from.
    pub(crate) fn resolve(&self, name: &str) -> Result<(GitHubSpec, DepKind)> {
        let mut spec = spec::parse(&self.source).with_context(|| format!("dependency '{name}'"))?;
        if let Some(git_ref) = &self.git_ref {
            spec.git_ref = Some(git_ref.clone());
        }
        let kind = match self.kind {
            Some(kind) => kind,
            None => DepKind::infer(&spec.path).with_context(|| {
                format!(
                    "dependency '{name}': cannot infer a kind from '{}'; set 'kind: workflow|agent|skill'",
                    spec.path
                )
            })?,
        };
        Ok((spec, kind))
    }
}

/// The parsed manifest. `deps` is a `BTreeMap` so `save_manifest`'s output
/// is stably ordered — the file is meant to be committed and diffed.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DepsManifest {
    #[serde(default = "default_version")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) deps: BTreeMap<String, DepRequirement>,
}

fn default_version() -> u32 {
    MANIFEST_VERSION
}

impl Default for DepsManifest {
    /// A fresh manifest (`deps add` creating `lait.deps.yml` in the current
    /// directory) starts at the current format version, matching what
    /// deserialization of a `version`-less file yields.
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            deps: BTreeMap::new(),
        }
    }
}

impl DepsManifest {
    /// `pub(crate)` so `schema::tests` can check `schemas/deps.json`
    /// against the real parser (the same cross-check the workflow/config/
    /// agent schemas get). `path` only feeds error messages — callers
    /// parsing in-memory text pass a display name like `"lait.deps.yml"`.
    pub(crate) fn parse(path: &Path, contents: &str) -> Result<Self> {
        let manifest: Self = serde_yaml::from_str(contents)
            .with_context(|| format!("failed to parse dependency manifest '{}'", path.display()))?;
        if manifest.version > MANIFEST_VERSION {
            bail!(
                "'{}' declares version {} but this lait understands up to {MANIFEST_VERSION}",
                path.display(),
                manifest.version,
            );
        }
        for name in manifest.deps.keys() {
            spec::validate_name(name).with_context(|| format!("in '{}'", path.display()))?;
        }
        Ok(manifest)
    }
}

/// A manifest plus the directory it was found in — the anchor every other
/// path in the deps feature is relative to (`lait.lock` beside it, payload
/// files under `.lait/deps/` inside it).
#[derive(Debug)]
pub(crate) struct ManifestLocation {
    pub(crate) dir: PathBuf,
    pub(crate) manifest: DepsManifest,
}

impl ManifestLocation {
    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST_FILE_NAME)
    }
    /// `lait.lock`, beside the manifest.
    pub(crate) fn lock_path(&self) -> PathBuf {
        self.dir.join(super::lock::LOCK_FILE_NAME)
    }
    /// The directory dependency payload files are materialized into:
    /// `.lait/deps/` — already the project-local, conventionally gitignored
    /// runtime directory (`sessions/`/`runs/`/`cache/` live alongside it),
    /// so a fresh checkout materializes dependencies with `lait deps
    /// install` the same way it would rebuild a cache.
    pub(crate) fn deps_dir(&self) -> PathBuf {
        self.dir.join(".lait").join("deps")
    }
    /// Where dep `name`'s payload file lives once materialized:
    /// `.lait/deps/<name>/<basename>`, keeping the upstream file name.
    pub(crate) fn dep_dir(&self, name: &str) -> PathBuf {
        self.deps_dir().join(name)
    }
}

/// Finds the nearest `lait.deps.yml` walking ancestor directories up from
/// `start` — see the module doc for why this mirrors the config search.
/// `None` when no ancestor has one; `deps add` then creates it in the
/// current directory.
pub(crate) fn find_manifest_upward(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        let candidate = directory.join(MANIFEST_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Locates and loads the manifest governing the current directory, or
/// `None` when there is none (an absent manifest is simply "no
/// dependencies", not an error — callers that need one say so in their own
/// message).
pub(crate) fn load_manifest() -> Result<Option<ManifestLocation>> {
    let cwd = std::env::current_dir()
        .context("failed to determine the current directory for dependency lookup")?;
    let Some(path) = find_manifest_upward(&cwd) else {
        return Ok(None);
    };
    let contents = async_io::read_to_string_sync(&path)
        .with_context(|| format!("failed to read dependency manifest '{}'", path.display()))?;
    let manifest = DepsManifest::parse(&path, &contents)?;
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(Some(ManifestLocation { dir, manifest }))
}

/// Cancellation-aware counterpart to [`load_manifest`] for the async
/// `deps add`/`install`/`update` commands — the manifest read can hit a
/// FIFO or a slow filesystem like every other file this crate loads, so it
/// goes through the same bounded worker (`async_io::run_blocking`) rather
/// than blocking a Tokio executor thread.
pub(crate) async fn load_manifest_cancellable(
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<Option<ManifestLocation>> {
    async_io::run_blocking(
        move |cancelled| {
            let cwd = std::env::current_dir()
                .context("failed to determine the current directory for dependency lookup")?;
            let Some(path) = (|| {
                use std::sync::atomic::Ordering;
                for directory in cwd.ancestors() {
                    if cancelled.load(Ordering::Acquire) {
                        return None;
                    }
                    let candidate = directory.join(MANIFEST_FILE_NAME);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
                None
            })() else {
                return Ok(None);
            };
            let contents = async_io::read_to_string_wait_for_fifo_writer(
                &path,
                cancelled,
                async_io::MAX_READ_BYTES,
            )
            .with_context(|| format!("failed to read dependency manifest '{}'", path.display()))?;
            let manifest = DepsManifest::parse(&path, &contents)?;
            let dir = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            Ok(Some(ManifestLocation { dir, manifest }))
        },
        cancellation,
    )
    .await
}

/// Atomically rewrites `lait.deps.yml` at `location` with its in-memory
/// contents. The file is tool-managed (comments are not preserved), which
/// is exactly why dependencies live in their own manifest instead of
/// inside the hand-edited `lait.config.yml`.
pub(crate) fn save_manifest(location: &ManifestLocation) -> Result<()> {
    let contents = serde_yaml::to_string(&location.manifest)
        .context("failed to serialize the dependency manifest")?;
    storage::write_atomic(&location.manifest_path(), contents.as_bytes())
}
