//! Shared read/write primitives for an append-only, newline-delimited JSON
//! log — the on-disk shape both `history` (a single user-wide log) and
//! `session` (one log per named session) use.

use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

/// Parent directories `ensure_dir` has already confirmed exist, so a
/// long-running caller appending one record at a time (a REPL turn, one
/// `--session`/history write each) doesn't pay a `create_dir_all` (a failed
/// `mkdir` plus a `stat`) on every single call once the directory is there.
/// Keyed by absolute path — `session`'s log directory is `cwd`-relative, so a
/// bare relative `Path` would collide across two different working
/// directories (exactly what happens between two of this module's own
/// per-test temp directories); resolving through `current_dir` first keeps
/// the cache correct even though it costs its own syscall, since a `getcwd`
/// is still cheaper than the `mkdir`+`stat` pair it lets later calls skip.
static ENSURED_DIRS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn ensure_dir(parent: &Path) -> Result<()> {
    let key = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to determine the current directory")?
            .join(parent)
    };
    let mut ensured = ENSURED_DIRS
        .lock()
        .expect("ensured-dirs lock should not be poisoned");
    if ensured.contains(&key) {
        return Ok(());
    }
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory '{}'", parent.display()))?;
    ensured.insert(key);
    Ok(())
}

/// Appends every record in `records` to the log at `path`, creating its
/// parent directory on first use.
pub(crate) fn append(path: &Path, records: impl IntoIterator<Item = impl Serialize>) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open '{}'", path.display()))?;
    for record in records {
        let line = serde_json::to_string(&record).context("failed to serialize a log entry")?;
        writeln!(file, "{line}")
            .with_context(|| format!("failed to write to '{}'", path.display()))?;
    }
    Ok(())
}

/// `path`'s contents, or an empty string when it doesn't exist yet — the
/// common case for a log that's never been written to. Shared by [`load`]
/// and [`count_lines`].
fn read_or_empty(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read '{}'", path.display())),
    }
}

/// Loads every record from the log at `path`, in the order they were
/// appended. Returns an empty `Vec` (not an error) when the file doesn't
/// exist yet.
pub(crate) fn load<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    read_or_empty(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .with_context(|| format!("failed to parse a line of '{}'", path.display()))
        })
        .collect()
}

/// The number of non-empty lines in the log at `path` — cheaper than
/// [`load`] when a caller only needs a count (e.g. `session::count_turns`).
pub(crate) fn count_lines(path: &Path) -> Result<usize> {
    Ok(read_or_empty(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
}

#[cfg(test)]
mod tests {
    use super::append;

    /// Regression test for `ensure_dir`'s cache key: `session`'s log
    /// directory is `cwd`-relative, so the same relative parent path
    /// (`"relative/dir"` here) legitimately needs creating again under a
    /// second, different working directory. A cache keyed by the bare
    /// relative `Path` would wrongly think it already exists (left behind by
    /// the first `in_temp_dir` below) and skip `create_dir_all`, making the
    /// second `append` fail to open its file.
    #[test]
    fn append_creates_the_parent_directory_under_two_different_working_directories() {
        let log = std::path::Path::new("relative/dir/log.jsonl");
        crate::test_support::in_temp_dir("lait-test-jsonl-a", || {
            append(log, [serde_json::json!({"n": 1})]).unwrap();
            assert!(log.exists());
        });
        crate::test_support::in_temp_dir("lait-test-jsonl-b", || {
            append(log, [serde_json::json!({"n": 2})]).unwrap();
            assert!(log.exists());
        });
    }
}
