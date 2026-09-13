//! Unix-only, symlink-safe relative-path primitives for `jsonl`'s
//! `#[cfg(unix)]` half of its six `_relative` function pairs (`append`,
//! `open`, `path_exists`, `directory_exists`, `remove`, `read_dir` here —
//! see `super`'s doc comment for why these aren't unified behind a trait
//! with the `#[cfg(not(unix))]` half). Every path component is resolved one
//! directory at a time via `openat`/`fstatat` with `O_NOFOLLOW`, so a
//! symlink anywhere along the path — not just at the final component — is
//! rejected before it can be followed.

use super::{
    RelativeDirEntry, inspect_context, open_context, open_parent_context, read_context,
    remove_context, write_context,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::{
    ffi::{CStr, CString, OsString},
    fs::File,
    io::{self, Write},
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

fn check_final(directory: &File, name: &CString, path: &Path) -> io::Result<Option<libc::stat>> {
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

pub(super) fn append(path: &Path, records: impl IntoIterator<Item = impl Serialize>) -> Result<()> {
    let (directory, name) = open_parent(path, true).with_context(|| open_parent_context(path))?;
    check_final(&directory, &name, path).with_context(|| inspect_context(path))?;
    let mut file =
        open_file_at(&directory, &name, APPEND_FLAGS, 0o666).with_context(|| open_context(path))?;
    for record in records {
        let line = serde_json::to_string(&record).context("failed to serialize a log entry")?;
        writeln!(file, "{line}").with_context(|| write_context(path))?;
    }
    Ok(())
}

/// Opens `path` for reading through the no-follow-symlink `openat`
/// sequence every other function in this module uses, without reading
/// its contents — the `File`-returning core `super::read_or_empty`
/// (a whole-file read) and `super::open_or_none`'s reverse-line reader
/// (a bounded read from the end — see its doc comment) both build on.
/// `None` means "doesn't exist yet", not an error.
pub(super) fn open(path: &Path) -> Result<Option<File>> {
    let (directory, name) = match open_parent(path, false) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| open_parent_context(path));
        }
    };
    let Some(_) = check_final(&directory, &name, path).with_context(|| inspect_context(path))?
    else {
        return Ok(None);
    };
    let file =
        open_file_at(&directory, &name, READ_FLAGS, 0).with_context(|| read_context(path))?;
    Ok(Some(file))
}

pub(super) fn path_exists(path: &Path) -> Result<bool> {
    let (directory, name) = match open_parent(path, false) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| open_parent_context(path));
        }
    };
    Ok(check_final(&directory, &name, path)
        .with_context(|| inspect_context(path))?
        .is_some())
}

pub(super) fn directory_exists(path: &Path) -> Result<bool> {
    let Some((directory, name)) = (match open_parent(path, false) {
        Ok(value) => Some(value),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| open_parent_context(path));
        }
    }) else {
        return Ok(false);
    };
    let Some(stat) = check_final(&directory, &name, path).with_context(|| inspect_context(path))?
    else {
        return Ok(false);
    };
    if (stat.st_mode as libc::mode_t & libc::S_IFMT) != libc::S_IFDIR {
        bail!("expected '{}' to be a directory", path.display());
    }
    Ok(true)
}

pub(super) fn remove(path: &Path) -> Result<()> {
    let (directory, name) = open_parent(path, false).with_context(|| open_parent_context(path))?;
    if check_final(&directory, &name, path)
        .with_context(|| inspect_context(path))?
        .is_none()
    {
        return Err(io::Error::new(io::ErrorKind::NotFound, "file not found").into());
    }
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result < 0 {
        return Err(io::Error::last_os_error()).with_context(|| remove_context(path));
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
