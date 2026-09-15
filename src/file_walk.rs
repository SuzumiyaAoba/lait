//! Recursive directory walking shared by `lait lint`'s and `lait test`'s
//! target expansion — both used to implement the same traversal by hand:
//! a deterministic (path-sorted) `read_dir` order, no symlink following,
//! `.`-prefixed entries and `storage::SKIPPED_DIR_NAMES` skipped, and a
//! canonicalized visited-directory set so overlapping explicit arguments or
//! a nested repeat never walk a directory twice.
//!
//! One deliberate normalization this module carries: `lait lint`'s old
//! walker only skipped `.`-prefixed *directories*, so a `.hidden.yml` inside
//! a walked directory was still linted; the shared rule skips `.`-prefixed
//! entries of every kind (matching `lait test`'s existing behavior and the
//! spirit of the dot-directory skip). Explicitly named file arguments are
//! unaffected — the walker only ever sees directories.
//!
//! The walker yields each regular file through a caller-supplied `on_file`
//! callback rather than collecting them itself: `lait lint` keeps verbatim
//! paths plus its `.yml`/`.yaml`/frontmatter-`.md` filter, while `lait test`
//! canonicalize-deduplicates each discovered `.yml`/`.yaml` file — a policy
//! difference the walk itself must not flatten away.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};

use crate::storage::{self, SKIPPED_DIR_NAMES};

/// The traversal half of target expansion. Owns the visited-directory set so
/// it persists across a caller's several explicit directory arguments, and
/// polls `cancelled` between bounded filesystem operations so `lait test`'s
/// worker-side flag can interrupt a large tree mid-walk. A caller with no
/// cancellation source (`lait lint`, which runs before any async runtime
/// exists) passes `&crate::cancellation::NEVER_SET`.
pub(crate) struct DirWalker<'a> {
    cancelled: &'a AtomicBool,
    cancel_message: &'static str,
    visited_directories: HashSet<PathBuf>,
}

impl<'a> DirWalker<'a> {
    pub(crate) fn new(cancelled: &'a AtomicBool, cancel_message: &'static str) -> Self {
        Self {
            cancelled,
            cancel_message,
            visited_directories: HashSet::new(),
        }
    }

    /// Recursively walks `dir`, calling `on_file` once per discovered
    /// regular file. Entries are visited in deterministic path order; the
    /// file itself decides nothing — `on_file` applies the caller's own
    /// filter (extension, frontmatter sniffing, dedup) and may bail to abort
    /// the walk with that error.
    pub(crate) fn walk(
        &mut self,
        dir: &Path,
        on_file: &mut impl FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        crate::cancellation::check_flag(self.cancelled, self.cancel_message)?;
        let identity = std::fs::canonicalize(dir)
            .with_context(|| format!("failed to resolve directory '{}'", dir.display()))?;
        if !self.visited_directories.insert(identity) {
            return Ok(());
        }

        let mut entries = std::fs::read_dir(dir)
            .with_context(|| storage::read_dir_context(dir))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| storage::read_dir_context(dir))?;
        // Deterministic traversal order, so directory expansion is stable
        // across runs/platforms (relied on by tests, and friendlier for CI
        // diffs than filesystem-dependent order).
        entries.sort_by_key(std::fs::DirEntry::path);

        for entry in entries {
            crate::cancellation::check_flag(self.cancelled, self.cancel_message)?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            // `DirEntry::file_type` inspects the entry itself rather than
            // its target, so this branch can never follow a symlink into a
            // recursive walk or hand a symlinked file to `on_file`.
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect '{}'", path.display()))?;
            if file_type.is_symlink() || (!file_type.is_file() && !file_type.is_dir()) {
                continue;
            }
            if file_type.is_dir() {
                if SKIPPED_DIR_NAMES.contains(&name.as_ref()) {
                    continue;
                }
                self.walk(&path, on_file)?;
            } else {
                on_file(&path)?;
            }
        }
        Ok(())
    }
}
