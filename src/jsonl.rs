//! Shared read/write primitives for an append-only, newline-delimited JSON
//! log — the on-disk shape both `history` (a single user-wide log) and
//! `session` (one log per named session) use.

use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

/// Appends every record in `records` to the log at `path`, creating its
/// parent directory on first use.
pub(crate) fn append(path: &Path, records: impl IntoIterator<Item = impl Serialize>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory '{}'", parent.display()))?;
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
