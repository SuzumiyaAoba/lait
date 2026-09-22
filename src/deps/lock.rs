//! `lait.lock` — the generated half of the dependency state, sitting beside
//! `lait.deps.yml` (see `super::manifest`). Where the manifest records what
//! was *requested* (a source and an optional ref), the lock records what
//! was actually *resolved* the last time `deps add`/`install`/`update`
//! fetched it: the commit SHA the ref pointed at and the SHA-256 of the
//! downloaded payload. `lait deps install` then re-materializes exactly
//! those bytes without re-resolving the ref — the pinning the user asked
//! for — and `lait deps verify` can detect a locally modified payload.
//!
//! The file is tool-managed YAML (committed alongside the manifest, meant
//! to be diffed), written atomically via `storage::write_atomic` like every
//! other generated file in this crate.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{async_io, storage};

use super::manifest::ManifestLocation;
use super::spec::{DepKind, GitHubSpec};

/// The lock file's own name, always beside [`super::manifest::MANIFEST_FILE_NAME`].
pub(crate) const LOCK_FILE_NAME: &str = "lait.lock";

/// The on-disk format version written by this build — see
/// `manifest::MANIFEST_VERSION` for the rejection policy.
pub(crate) const LOCK_VERSION: u32 = 1;

/// One locked dependency: what `deps` resolved and fetched for `name`.
/// `repo`/`path`/`ref`/`kind` together are the *request identity* — an
/// entry only covers a manifest entry when all four match, so editing any
/// of them in `lait.deps.yml` is what makes the lock stale (and what
/// `install --frozen` rejects).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LockedDep {
    pub(crate) kind: DepKind,
    /// `"owner/repo"`.
    pub(crate) repo: String,
    /// The in-repo path of the fetched file.
    pub(crate) path: String,
    /// The requested ref this resolution came from (`None` — serialized as
    /// absent — means "the repository's default branch", matching the
    /// manifest's own `ref` semantics, so a drift check can compare the two
    /// directly without re-resolving the default branch over the network).
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_ref: Option<String>,
    /// The commit SHA `git_ref` (or the default branch) resolved to.
    pub(crate) commit: String,
    /// Lowercase hex SHA-256 of the payload bytes written to `file`.
    pub(crate) sha256: String,
    /// The materialized payload, relative to the manifest's directory
    /// (`.lait/deps/<name>/<basename>`) so a committed lock stays
    /// machine-independent.
    pub(crate) file: String,
}

impl LockedDep {
    /// Whether this entry still covers `req`'s request — see the struct doc
    /// for the identity rule.
    pub(crate) fn matches(&self, spec: &GitHubSpec, kind: DepKind) -> bool {
        self.kind == kind
            && self.repo == spec.repo_slug()
            && self.path == spec.path
            && self.git_ref == spec.git_ref
    }

    /// The payload's absolute path: `file` resolved against the manifest
    /// directory (see the field's own comment).
    pub(crate) fn absolute_file(&self, location: &ManifestLocation) -> std::path::PathBuf {
        location.dir.join(&self.file)
    }
}

/// The parsed lock file. Absent is not an error — [`load_lock`] maps a
/// missing file to an empty lock, since only `install --frozen` treats
/// "nothing locked yet" as a failure.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LockFile {
    #[serde(default = "default_version")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) deps: BTreeMap<String, LockedDep>,
}

fn default_version() -> u32 {
    LOCK_VERSION
}

impl LockFile {
    /// The lock's own hash function for payload content — one spelling so
    /// `install`/`verify`/`add` can never drift on the digest format
    /// (lowercase hex, `cache.rs`'s `{:x}` convention).
    pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Hashes the already-materialized file at `path` the same way
    /// [`Self::sha256_hex`] hashes fetched bytes — `verify` and `install`'s
    /// skip check compare the two.
    pub(crate) fn sha256_file(path: &Path) -> Result<String> {
        let contents =
            std::fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
        Ok(Self::sha256_hex(&contents))
    }
}

/// Loads `lait.lock` beside `location`'s manifest; a missing file yields an
/// empty lock, a present-but-unparseable one an error.
pub(crate) fn load_lock(location: &ManifestLocation) -> Result<LockFile> {
    let path = location.lock_path();
    match async_io::read_to_string_sync(&path) {
        Ok(contents) => {
            let lock: LockFile = serde_yaml::from_str(&contents).with_context(|| {
                format!("failed to parse dependency lock file '{}'", path.display())
            })?;
            if lock.version > LOCK_VERSION {
                bail!(
                    "'{}' declares version {} but this lait understands up to {LOCK_VERSION}",
                    path.display(),
                    lock.version,
                );
            }
            Ok(lock)
        }
        Err(error) if async_io::is_not_found(&error) => Ok(LockFile::default()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read dependency lock file '{}'", path.display())),
    }
}

/// Atomically rewrites `lait.lock` at `location` — see the module doc.
pub(crate) fn save_lock(location: &ManifestLocation, lock: &LockFile) -> Result<()> {
    let contents =
        serde_yaml::to_string(lock).context("failed to serialize the dependency lock file")?;
    storage::write_atomic(&location.lock_path(), contents.as_bytes())
}
