//! Shared read/write primitives for an append-only, newline-delimited JSON
//! log — the on-disk shape both `history` (a single user-wide log) and
//! `session` (one log per named session) use.

use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

/// Appends every record in `records` to the log at `path`, creating its
/// parent directory on first use. `kind` (e.g. `"history"`, `"session"`)
/// names the log in error messages.
pub(crate) fn append(
    path: &Path,
    records: impl IntoIterator<Item = impl Serialize>,
    kind: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {kind} directory '{}'", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {kind} file '{}'", path.display()))?;
    for record in records {
        let line = serde_json::to_string(&record)
            .with_context(|| format!("failed to serialize a {kind} entry"))?;
        writeln!(file, "{line}")
            .with_context(|| format!("failed to write to {kind} file '{}'", path.display()))?;
    }
    Ok(())
}

/// Loads every record from the log at `path`, in the order they were
/// appended. Returns an empty `Vec` (not an error) when the file doesn't
/// exist yet — the common case for a log that's never been written to.
/// `kind` names the log in error messages.
pub(crate) fn load<T: DeserializeOwned>(path: &Path, kind: &str) -> Result<Vec<T>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {kind} file '{}'", path.display()));
        }
    };
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).with_context(|| {
                format!("failed to parse a line of {kind} file '{}'", path.display())
            })
        })
        .collect()
}
