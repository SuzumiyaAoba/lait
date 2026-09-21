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

#[cfg(test)]
mod tests {
    use super::DirWalker;
    use crate::cancellation::NEVER_SET;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn walk_collecting(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut walker = DirWalker::new(&NEVER_SET, "test walk was cancelled");
        let mut found = Vec::new();
        walker
            .walk(dir, &mut |path| {
                found.push(path.to_path_buf());
                Ok(())
            })
            .unwrap();
        found
    }

    #[test]
    fn walks_files_in_deterministic_path_order() {
        crate::test_support::in_temp_dir("lait-test-file-walk-order", || {
            // Intentionally created out of alphabetical order — the walker's
            // own `entries.sort_by_key(DirEntry::path)` must produce a
            // deterministic result regardless of `read_dir`'s own order.
            std::fs::write("z.txt", "").unwrap();
            std::fs::create_dir_all("m").unwrap();
            std::fs::write("m/inner.txt", "").unwrap();
            std::fs::write("a.txt", "").unwrap();

            let found = walk_collecting(std::path::Path::new("."));

            assert_eq!(
                found,
                vec![
                    PathBuf::from("./a.txt"),
                    PathBuf::from("./m/inner.txt"),
                    PathBuf::from("./z.txt"),
                ]
            );
        });
    }

    #[test]
    fn skips_dot_prefixed_entries_of_every_kind() {
        crate::test_support::in_temp_dir("lait-test-file-walk-dot", || {
            std::fs::write("visible.txt", "").unwrap();
            std::fs::write(".hidden.txt", "").unwrap();
            std::fs::create_dir_all(".hidden_dir").unwrap();
            std::fs::write(".hidden_dir/inner.txt", "").unwrap();

            let found = walk_collecting(std::path::Path::new("."));

            assert_eq!(found, vec![PathBuf::from("./visible.txt")]);
        });
    }

    #[test]
    fn skips_storages_skipped_dir_names() {
        crate::test_support::in_temp_dir("lait-test-file-walk-skipped-dirs", || {
            std::fs::write("top.txt", "").unwrap();
            std::fs::create_dir_all("target").unwrap();
            std::fs::write("target/build.txt", "").unwrap();
            std::fs::create_dir_all("node_modules/pkg").unwrap();
            std::fs::write("node_modules/pkg/dep.txt", "").unwrap();

            let found = walk_collecting(std::path::Path::new("."));

            assert_eq!(found, vec![PathBuf::from("./top.txt")]);
        });
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_a_symlinked_directory() {
        crate::test_support::in_temp_dir("lait-test-file-walk-symlink-dir", || {
            std::fs::create_dir_all("real").unwrap();
            std::fs::write("real/inside.txt", "").unwrap();
            std::os::unix::fs::symlink("real", "link_to_real").unwrap();

            let found = walk_collecting(std::path::Path::new("."));

            // `link_to_real` itself is a symlink `DirEntry` (`file_type` on
            // the entry, not its target — see `walk`'s comment), so the
            // walker's `is_symlink() || (!is_file() && !is_dir())` guard
            // skips it outright instead of recursing through it: only the
            // real directory's own contents are ever yielded.
            assert_eq!(found, vec![PathBuf::from("./real/inside.txt")]);
        });
    }

    #[cfg(unix)]
    #[test]
    fn does_not_yield_a_symlinked_file() {
        crate::test_support::in_temp_dir("lait-test-file-walk-symlink-file", || {
            std::fs::write("real.txt", "").unwrap();
            std::os::unix::fs::symlink("real.txt", "link.txt").unwrap();

            let found = walk_collecting(std::path::Path::new("."));

            assert_eq!(found, vec![PathBuf::from("./real.txt")]);
        });
    }

    #[test]
    fn a_directory_argument_reached_twice_is_only_walked_once() {
        crate::test_support::in_temp_dir("lait-test-file-walk-dedup", || {
            std::fs::create_dir_all("sub").unwrap();
            std::fs::write("sub/inner.txt", "").unwrap();

            let mut walker = DirWalker::new(&NEVER_SET, "test walk was cancelled");
            let mut found = Vec::new();
            // Same directory, two different (but canonically identical)
            // spellings — `visited_directories` is keyed by canonical
            // identity, not by the literal `Path` passed in.
            walker
                .walk(std::path::Path::new("sub"), &mut |path| {
                    found.push(path.to_path_buf());
                    Ok(())
                })
                .unwrap();
            walker
                .walk(std::path::Path::new("./sub"), &mut |path| {
                    found.push(path.to_path_buf());
                    Ok(())
                })
                .unwrap();

            assert_eq!(found, vec![PathBuf::from("sub/inner.txt")]);
        });
    }

    #[test]
    fn a_cancelled_flag_stops_the_walk_with_a_typed_interruption() {
        crate::test_support::in_temp_dir("lait-test-file-walk-cancel", || {
            std::fs::write("a.txt", "").unwrap();

            let cancelled = AtomicBool::new(true);
            let mut walker = DirWalker::new(&cancelled, "test walk was cancelled");
            let error = walker
                .walk(std::path::Path::new("."), &mut |_path| Ok(()))
                .unwrap_err();

            assert!(
                crate::error::is_interrupted(&error),
                "a pre-tripped flag must surface as a typed interruption, not an ordinary error: {error}"
            );
        });
    }

    #[test]
    fn a_flag_tripped_mid_walk_stops_before_a_later_directory() {
        crate::test_support::in_temp_dir("lait-test-file-walk-cancel-mid", || {
            std::fs::create_dir_all("a").unwrap();
            std::fs::write("a/one.txt", "").unwrap();
            std::fs::create_dir_all("b").unwrap();
            std::fs::write("b/two.txt", "").unwrap();

            let cancelled = AtomicBool::new(false);
            let mut walker = DirWalker::new(&cancelled, "test walk was cancelled");
            let mut found = Vec::new();
            let error = walker
                .walk(std::path::Path::new("."), &mut |path| {
                    found.push(path.to_path_buf());
                    // Trip the flag right after the first (path-sorted)
                    // file, so the walk's own next `check_flag` call (before
                    // descending into `b/`) is what stops it.
                    cancelled.store(true, Ordering::Release);
                    Ok(())
                })
                .unwrap_err();

            assert!(crate::error::is_interrupted(&error));
            assert_eq!(found, vec![PathBuf::from("./a/one.txt")]);
        });
    }
}
