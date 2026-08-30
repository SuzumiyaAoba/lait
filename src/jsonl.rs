//! Shared read/write primitives for an append-only, newline-delimited JSON
//! log — the on-disk shape both `history` (a single user-wide log) and
//! `session` (one log per named session) use.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

#[cfg(not(unix))]
use std::fs::Metadata;

#[cfg(not(unix))]
use anyhow::anyhow;
use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};

#[cfg(not(unix))]
fn refusing_symlink(path: &Path) -> anyhow::Error {
    anyhow!("refusing to follow symbolic link '{}'", path.display())
}

#[derive(Debug)]
pub(crate) struct RelativeDirEntry {
    pub(crate) name: OsString,
    pub(crate) is_symlink: bool,
}

#[cfg(unix)]
mod unix_relative {
    use super::RelativeDirEntry;
    use anyhow::{Context, Result, anyhow, bail};
    use serde::Serialize;
    use std::{
        ffi::{CStr, CString, OsString},
        fs::File,
        io::{self, Read, Write},
        mem::MaybeUninit,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::{OsStrExt, OsStringExt},
        },
        path::{Component, Path},
    };

    const DIRECTORY_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    const READ_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    const APPEND_FLAGS: libc::c_int =
        libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    fn invalid_path(path: &Path, reason: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("relative log path '{}' {reason}", path.display()),
        )
    }

    fn component_name(path: &Path, component: Component<'_>) -> io::Result<CString> {
        CString::new(component.as_os_str().as_bytes())
            .map_err(|_| invalid_path(path, "contains a NUL byte"))
    }

    fn relative_names(path: &Path) -> io::Result<Vec<CString>> {
        if path.is_absolute() {
            return Err(invalid_path(path, "must not be absolute"));
        }
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(_) => names.push(component_name(path, component)?),
                Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                    return Err(invalid_path(
                        path,
                        "must not contain a parent, prefix, or root component",
                    ));
                }
            }
        }
        Ok(names)
    }

    fn open_current_dir() -> io::Result<File> {
        let name = CString::new(".").expect("literal has no NUL");
        let fd = unsafe { libc::open(name.as_ptr(), DIRECTORY_FLAGS) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn open_dir_at(parent: &File, name: &CString) -> io::Result<File> {
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), DIRECTORY_FLAGS) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn open_child_dir(parent: &File, name: &CString, create: bool) -> io::Result<File> {
        let result = match open_dir_at(parent, name) {
            Ok(directory) => Ok(directory),
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) };
                if result < 0 {
                    let mkdir_error = io::Error::last_os_error();
                    if mkdir_error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(mkdir_error);
                    }
                }
                open_dir_at(parent, name)
            }
            Err(error) => Err(error),
        };
        match result {
            Err(_error)
                if stat_at(parent, name)
                    .map(|stat| is_symlink(&stat))
                    .unwrap_or(false) =>
            {
                Err(refusing_symlink())
            }
            other => other,
        }
    }

    fn open_directory(path: &Path, create: bool) -> io::Result<File> {
        let names = relative_names(path)?;
        let mut directory = open_current_dir()?;
        for name in names {
            directory = open_child_dir(&directory, &name, create)?;
        }
        Ok(directory)
    }

    fn open_parent(path: &Path, create: bool) -> io::Result<(File, CString)> {
        let names = relative_names(path)?;
        let (basename, parents) = names
            .split_last()
            .ok_or_else(|| invalid_path(path, "must name a file"))?;
        let mut directory = open_current_dir()?;
        for name in parents {
            directory = open_child_dir(&directory, name, create)?;
        }
        Ok((directory, basename.clone()))
    }

    fn open_file_at(
        directory: &File,
        name: &CString,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<File> {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ELOOP) {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to follow symbolic link",
                ))
            } else {
                Err(error)
            }
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn stat_at(directory: &File, name: &CString) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { stat.assume_init() })
        }
    }

    fn is_symlink(stat: &libc::stat) -> bool {
        (stat.st_mode as libc::mode_t & libc::S_IFMT) == libc::S_IFLNK
    }

    fn refusing_symlink() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to follow symbolic link",
        )
    }

    fn check_final(
        directory: &File,
        name: &CString,
        path: &Path,
    ) -> io::Result<Option<libc::stat>> {
        match stat_at(directory, name) {
            Ok(stat) if is_symlink(&stat) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing to follow symbolic link '{}'", path.display()),
            )),
            Ok(stat) => Ok(Some(stat)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn append(
        path: &Path,
        records: impl IntoIterator<Item = impl Serialize>,
    ) -> Result<()> {
        let (directory, name) = open_parent(path, true)
            .with_context(|| format!("failed to open parent of '{}'", path.display()))?;
        check_final(&directory, &name, path)
            .with_context(|| format!("failed to inspect '{}'", path.display()))?;
        let mut file = open_file_at(&directory, &name, APPEND_FLAGS, 0o666)
            .with_context(|| format!("failed to open '{}'", path.display()))?;
        for record in records {
            let line = serde_json::to_string(&record).context("failed to serialize a log entry")?;
            writeln!(file, "{line}")
                .with_context(|| format!("failed to write to '{}'", path.display()))?;
        }
        Ok(())
    }

    pub(super) fn read(path: &Path) -> Result<String> {
        let (directory, name) = match open_parent(path, false) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to open parent of '{}'", path.display()));
            }
        };
        let Some(_) = check_final(&directory, &name, path)
            .with_context(|| format!("failed to inspect '{}'", path.display()))?
        else {
            return Ok(String::new());
        };
        let mut file = open_file_at(&directory, &name, READ_FLAGS, 0)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        Ok(contents)
    }

    pub(super) fn path_exists(path: &Path) -> Result<bool> {
        let (directory, name) = match open_parent(path, false) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to open parent of '{}'", path.display()));
            }
        };
        Ok(check_final(&directory, &name, path)
            .with_context(|| format!("failed to inspect '{}'", path.display()))?
            .is_some())
    }

    pub(super) fn directory_exists(path: &Path) -> Result<bool> {
        let Some((directory, name)) = (match open_parent(path, false) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to open parent of '{}'", path.display()));
            }
        }) else {
            return Ok(false);
        };
        let Some(stat) = check_final(&directory, &name, path)
            .with_context(|| format!("failed to inspect '{}'", path.display()))?
        else {
            return Ok(false);
        };
        if (stat.st_mode as libc::mode_t & libc::S_IFMT) != libc::S_IFDIR {
            bail!("expected '{}' to be a directory", path.display());
        }
        Ok(true)
    }

    pub(super) fn remove(path: &Path) -> Result<()> {
        let (directory, name) = open_parent(path, false)
            .with_context(|| format!("failed to open parent of '{}'", path.display()))?;
        if check_final(&directory, &name, path)
            .with_context(|| format!("failed to inspect '{}'", path.display()))?
            .is_none()
        {
            return Err(io::Error::new(io::ErrorKind::NotFound, "file not found").into());
        }
        let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
        if result < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("failed to remove '{}'", path.display()));
        }
        Ok(())
    }

    pub(super) fn read_dir(path: &Path) -> Result<Vec<RelativeDirEntry>> {
        let directory = match open_directory(path, false) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to open directory '{}'", path.display()));
            }
        };
        let duplicate_fd = unsafe { libc::dup(directory.as_raw_fd()) };
        if duplicate_fd < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("failed to duplicate directory '{}'", path.display()));
        }
        let directory_stream = unsafe { libc::fdopendir(duplicate_fd) };
        if directory_stream.is_null() {
            unsafe {
                libc::close(duplicate_fd);
            }
            return Err(io::Error::last_os_error())
                .with_context(|| format!("failed to enumerate directory '{}'", path.display()));
        }

        let mut entries = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(directory_stream) };
            if entry.is_null() {
                break;
            }
            let name_bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr().cast()).to_bytes() };
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            let name = OsString::from_vec(name_bytes.to_vec());
            let name_c = CString::new(name_bytes).map_err(|_| {
                anyhow!(
                    "failed to inspect an entry of directory '{}'",
                    path.display()
                )
            })?;
            let stat = match stat_at(&directory, &name_c) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    unsafe {
                        libc::closedir(directory_stream);
                    }
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect an entry of directory '{}'",
                            path.display()
                        )
                    });
                }
            };
            entries.push(RelativeDirEntry {
                name,
                is_symlink: is_symlink(&stat),
            });
        }
        unsafe {
            libc::closedir(directory_stream);
        }
        Ok(entries)
    }
}

#[cfg(not(unix))]
use std::path::{Component, PathBuf};

#[cfg(not(unix))]
fn check_directory_metadata(path: &Path, metadata: &Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(refusing_symlink(path));
    }
    if !metadata.is_dir() {
        bail!("expected '{}' to be a directory", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_relative_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let mut current = PathBuf::new();
    for component in parent.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect directory component '{}'",
                        current.display()
                    )
                });
            }
        };
        check_directory_metadata(&current, &metadata)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_relative_dir(parent: &Path) -> Result<()> {
    check_relative_parent(&parent.join("session.jsonl"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory '{}'", parent.display()))?;
    check_relative_parent(&parent.join("session.jsonl"))
}

#[cfg(not(unix))]
fn check_final_path(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(refusing_symlink(path)),
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

fn open_for_read(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

fn open_for_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(unix)]
fn append_relative(path: &Path, records: impl IntoIterator<Item = impl Serialize>) -> Result<()> {
    unix_relative::append(path, records)
}

#[cfg(not(unix))]
// Windows keeps the existing symlink/reparse-point metadata checks. The
// standard library has no portable directory-handle/openat equivalent, so a
// concurrent replacement between inspection and the final operation cannot
// be made atomic here.
fn append_relative(path: &Path, records: impl IntoIterator<Item = impl Serialize>) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_relative_dir(parent)?;
    }
    check_final_path(path)?;
    let mut file =
        open_for_append(path).with_context(|| format!("failed to open '{}'", path.display()))?;
    for record in records {
        let line = serde_json::to_string(&record).context("failed to serialize a log entry")?;
        writeln!(file, "{line}")
            .with_context(|| format!("failed to write to '{}'", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn read_relative(path: &Path) -> Result<String> {
    unix_relative::read(path)
}

#[cfg(not(unix))]
fn read_relative(path: &Path) -> Result<String> {
    check_relative_parent(path)?;
    check_final_path(path)?;
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read '{}'", path.display())),
    }
}

#[cfg(unix)]
fn path_exists_relative(path: &Path) -> Result<bool> {
    unix_relative::path_exists(path)
}

#[cfg(not(unix))]
fn path_exists_relative(path: &Path) -> Result<bool> {
    check_relative_parent(path)?;
    Ok(check_final_path(path)?.is_some())
}

#[cfg(unix)]
fn directory_exists_relative(path: &Path) -> Result<bool> {
    unix_relative::directory_exists(path)
}

#[cfg(not(unix))]
fn directory_exists_relative(path: &Path) -> Result<bool> {
    check_relative_parent(path)?;
    let Some(metadata) = check_final_path(path)? else {
        return Ok(false);
    };
    check_directory_metadata(path, &metadata)?;
    Ok(true)
}

#[cfg(unix)]
fn remove_relative(path: &Path) -> Result<()> {
    unix_relative::remove(path)
}

#[cfg(not(unix))]
fn remove_relative(path: &Path) -> Result<()> {
    check_relative_parent(path)?;
    check_final_path(path)?;
    fs::remove_file(path).with_context(|| format!("failed to remove '{}'", path.display()))
}

#[cfg(unix)]
fn read_dir_relative(path: &Path) -> Result<Vec<RelativeDirEntry>> {
    unix_relative::read_dir(path)
}

#[cfg(not(unix))]
fn read_dir_relative(path: &Path) -> Result<Vec<RelativeDirEntry>> {
    if !directory_exists_relative(path)? {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(path)
        .with_context(|| format!("failed to read directory '{}'", path.display()))?;
    entries
        .map(|entry| {
            let entry = entry.with_context(|| {
                format!("failed to read an entry of directory '{}'", path.display())
            })?;
            let file_type = entry.file_type().with_context(|| {
                format!(
                    "failed to inspect an entry of directory '{}'",
                    path.display()
                )
            })?;
            Ok(RelativeDirEntry {
                name: entry.file_name(),
                is_symlink: file_type.is_symlink(),
            })
        })
        .collect()
}

/// Appends every record in `records` to the log at `path`, creating its
/// parent directory on first use.
pub(crate) fn append(path: &Path, records: impl IntoIterator<Item = impl Serialize>) -> Result<()> {
    if path.is_relative() {
        return append_relative(path, records);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory '{}'", parent.display()))?;
    }
    let mut file =
        open_for_append(path).with_context(|| format!("failed to open '{}'", path.display()))?;
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
    if path.is_relative() {
        return read_relative(path);
    }
    let mut file = match open_for_read(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read '{}'", path.display()));
        }
    };
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    Ok(contents)
}

/// Returns whether a relative session path exists without following a
/// symlink. Absolute paths retain the existing path-based behavior.
pub(crate) fn path_exists(path: &Path) -> Result<bool> {
    if path.is_relative() {
        return path_exists_relative(path);
    }
    Ok(path.exists())
}

/// Returns whether path is an existing, non-symlink directory. A missing
/// directory is reported as false so callers can preserve their empty-state
/// behavior.
pub(crate) fn directory_exists(path: &Path) -> Result<bool> {
    if path.is_relative() {
        return directory_exists_relative(path);
    }
    Ok(path.is_dir())
}

/// Removes a regular session/log path only after checking its parent and final
/// component without following symlinks. session::delete performs the
/// user-facing missing-file message around this primitive.
pub(crate) fn remove(path: &Path) -> Result<()> {
    if path.is_relative() {
        return remove_relative(path);
    }
    fs::remove_file(path).with_context(|| format!("failed to remove '{}'", path.display()))
}

/// Lists a relative directory through the platform's no-follow path. Unix
/// uses a directory FD and fstatat, while other platforms retain the
/// pre-existing metadata check in read_dir_relative.
pub(crate) fn read_dir(path: &Path) -> Result<Vec<RelativeDirEntry>> {
    if path.is_relative() {
        return read_dir_relative(path);
    }
    bail!("relative directory path expected, got '{}'", path.display());
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
