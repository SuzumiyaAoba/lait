//! Discovery, loading, and merging of the project and global config files.
//! Loading comes in synchronous ([`load_config`], for runtime-free commands
//! like `lint`) and cancellation-aware ([`load_config_cancellable`], for
//! async command entry points) forms that share one parsing core
//! ([`parse_config_file`]) — see `async_io::read_to_string_sync`'s doc for
//! why every synchronous reader in this file goes through it rather than
//! `std::fs::read_to_string` directly.

use std::{
    collections::HashMap,
    hash::Hash,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::async_io;

use super::types::{ConfigFile, DefaultSettings, ToolPolicy};
use super::{CONFIG_FILE_NAME, ConfigSource};

fn find_config_upward(start: &Path) -> Option<PathBuf> {
    // Delegates to the cancellable walk with a flag that is never set, so
    // the two never drift on which directory wins — a synchronous caller
    // has no cancellation channel to check in the first place, so this is
    // exactly the cancellable loop with every check compiled down to "never
    // trips". The flag being always-false means the walk can only ever
    // return `Ok`, never the `Err` its cancellation check would produce.
    find_config_upward_cancellable(start, &std::sync::atomic::AtomicBool::new(false))
        .expect("a cancellation flag that is never set cannot trip the cancellation check")
}

/// Resolves `source` to a concrete file path to read, or `None` when there is
/// none (`Disabled`, or `Search` that found nothing) — the information
/// `lint::run` needs to tell "no config anywhere" from "found one" apart from
/// [`load_config`]'s own `ConfigFile::default()` fallback, which looks the
/// same in both cases.
pub(crate) fn resolve_config_path(source: &ConfigSource) -> Result<Option<PathBuf>> {
    match source {
        ConfigSource::Disabled => Ok(None),
        ConfigSource::Explicit(path) => Ok(Some(path.clone())),
        ConfigSource::Search => {
            let cwd = std::env::current_dir()
                .context("failed to determine the current directory for configuration")?;
            Ok(find_config_upward(&cwd))
        }
    }
}

/// Resolves every `workflows:`/`agents:`/`skills:` registry path in `config`
/// against `config_dir` — the directory containing the `lait.config.yml` (or
/// global `config.yml`) it was parsed from — replacing each configured
/// (possibly relative) value with an absolute one. Called once, right after
/// parsing (see [`parse_config_file`]), rather than at each of
/// `resolve_run_target`/`lint::check_workflows_registry`/`workflow::list`/
/// `skill::list`/`subagent::list`'s use sites: with a project config
/// potentially merged with a global one (see [`load_config`]/
/// [`merge_config`]), a registry entry's path can no longer carry its own
/// origin directory alongside it once the two maps are combined, so it has
/// to already be absolute by then. Kept relative to the config file rather
/// than the current working directory so a registry entry keeps resolving to
/// the same file regardless of which subdirectory `lait` is invoked from,
/// the same way `lait.config.yml` itself is found by walking upward.
/// `Path::join` leaves an already-absolute value untouched, so this is
/// idempotent.
fn resolve_registry_paths_in_place(config: &mut ConfigFile, config_dir: &Path) {
    for path in config.workflows.values_mut() {
        *path = config_dir.join(&path);
    }
    for path in config.agents.values_mut() {
        *path = config_dir.join(&path);
    }
    for path in config.skills.values_mut() {
        *path = config_dir.join(&path);
    }
}

/// Resolves `$XDG_CONFIG_HOME`, falling back to `$HOME/.config` (or
/// `%USERPROFILE%\.config` where `HOME` isn't set) per the XDG Base
/// Directory spec — mirrors `history::xdg_data_home`'s reasoning for
/// avoiding a `dirs`-style crate (which would map to a platform-conventional
/// directory, e.g. `~/Library/Application Support` on macOS, rather than the
/// literal `~/.config` this feature is specified against).
fn xdg_config_home() -> Result<PathBuf> {
    crate::xdg::base_dir(
        "XDG_CONFIG_HOME",
        &[".config"],
        "the global configuration file",
    )
}

/// The global config file's path: `$XDG_CONFIG_HOME/lait/config.yml`. Read
/// (when it exists) by [`load_config`] for [`ConfigSource::Search`] only —
/// `--config PATH` reads exactly that file, and `--no-config` never calls
/// this at all.
pub(crate) fn global_config_path() -> Result<PathBuf> {
    Ok(xdg_config_home()?.join("lait").join("config.yml"))
}

fn merge_maps<K, V>(mut global: HashMap<K, V>, project: HashMap<K, V>) -> HashMap<K, V>
where
    K: Eq + Hash,
{
    global.extend(project);
    global
}

/// Merges `global` (loaded from [`global_config_path`]) with `project`
/// (found by [`ConfigSource::Search`]'s upward walk) into the single
/// `ConfigFile` every reader sees from here on, with `project` winning
/// wherever the two overlap. `models:`/`mcp_servers:`/`skills:`/`agents:`/
/// `prompts:`/`workflows:`/`tools:` merge key by key (a name defined in both
/// keeps the project definition); `default:` merges field by field the same
/// way; `base_url` keeps the project value when set, else falls back to the
/// global one. `api_key`/`api_key_cmd` merge as a single unit (whichever the
/// project sets, of either, wins as a pair) rather than falling back field
/// by field — see `DefaultSettings::merge`. `tool_policy`'s `allow`/`deny`
/// are unioned rather than key-by-key or project-wins — see `ToolPolicy::merge`
/// for why. Registry paths (`workflows:`/`agents:`/
/// `skills:`) are already absolute by this point (each was resolved by
/// `resolve_registry_paths_in_place` right after its own file was parsed —
/// see `parse_config_file`), so combining the two maps needs no
/// path-origin tracking.
fn merge_config(global: ConfigFile, project: ConfigFile) -> ConfigFile {
    // `api_key`/`api_key_cmd` are one logical "how do we get the top-level
    // key" choice, not two independently-falling-back fields — merging them
    // separately could pair the project's `api_key` with the global's
    // `api_key_cmd` (or vice versa), tripping `check_api_key_source`'s
    // both-set rejection even though neither file alone set both. Whichever
    // file actually set either field wins that file's whole pair.
    let (api_key, api_key_cmd) = if project.api_key.is_some() || project.api_key_cmd.is_some() {
        (project.api_key, project.api_key_cmd)
    } else {
        (global.api_key, global.api_key_cmd)
    };

    ConfigFile {
        base_url: project.base_url.or(global.base_url),
        api_key,
        api_key_cmd,
        default: DefaultSettings::merge(global.default, project.default),
        models: merge_maps(global.models, project.models),
        mcp_servers: merge_maps(global.mcp_servers, project.mcp_servers),
        skills: merge_maps(global.skills, project.skills),
        agents: merge_maps(global.agents, project.agents),
        prompts: merge_maps(global.prompts, project.prompts),
        workflows: merge_maps(global.workflows, project.workflows),
        tool_policy: ToolPolicy::merge(global.tool_policy, project.tool_policy),
        tools: merge_maps(global.tools, project.tools),
    }
}

/// Loads the config `resolve_request_settings`/every other reader sees:
/// [`ConfigSource::Search`] merges the project config (found by walking
/// upward from the current directory) with the global config at
/// [`global_config_path`] when that file exists (see [`merge_config`]) —
/// [`ConfigSource::Explicit`]/[`ConfigSource::Disabled`] never touch the
/// global file at all.
pub(crate) fn load_config(source: &ConfigSource) -> Result<ConfigFile> {
    let project = load_config_at(source, resolve_config_path(source)?)?;
    match source {
        ConfigSource::Search => match load_global_config()? {
            Some(global) => Ok(merge_config(global, project)),
            None => Ok(project),
        },
        ConfigSource::Explicit(_) | ConfigSource::Disabled => Ok(project),
    }
}

/// Cancellation-aware counterpart to [`load_config`] for async command entry
/// points. Configuration files can be FIFOs or live on a slow filesystem, so
/// both path discovery and file reads run through [`async_io::run_blocking`]
/// rather than blocking the Tokio runtime. The synchronous loader remains the
/// API for runtime-free commands such as `lint`, `init`, and registry listing.
pub(crate) async fn load_config_cancellable(
    source: &ConfigSource,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<ConfigFile> {
    let project_path = resolve_config_path_cancellable(source, cancellation.clone()).await?;
    let project = load_config_at_cancellable(source, project_path, cancellation.clone()).await?;
    match source {
        ConfigSource::Search => match load_global_config_cancellable(cancellation).await? {
            Some(global) => Ok(merge_config(global, project)),
            None => Ok(project),
        },
        ConfigSource::Explicit(_) | ConfigSource::Disabled => Ok(project),
    }
}

/// Cancellation-aware counterpart to [`resolve_config_path`]. Search walks
/// ancestor directories on the bounded filesystem worker so metadata checks
/// cannot block signal handling on the Tokio runtime. Explicit and disabled
/// sources have no filesystem work and return immediately.
pub(crate) async fn resolve_config_path_cancellable(
    source: &ConfigSource,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Option<PathBuf>> {
    if cancellation
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        bail!(crate::error::Interrupted::cancelled(
            "configuration lookup was cancelled"
        ));
    }
    match source {
        ConfigSource::Disabled => Ok(None),
        ConfigSource::Explicit(path) => Ok(Some(path.clone())),
        ConfigSource::Search => {
            async_io::run_blocking(
                move |cancelled| {
                    let cwd = std::env::current_dir()
                        .context("failed to determine the current directory for configuration")?;
                    find_config_upward_cancellable(&cwd, cancelled)
                },
                cancellation,
            )
            .await
        }
    }
}

fn find_config_upward_cancellable(
    start: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<PathBuf>> {
    use std::sync::atomic::Ordering;

    for directory in start.ancestors() {
        if cancelled.load(Ordering::Acquire) {
            bail!(crate::error::Interrupted::cancelled(
                "configuration lookup was cancelled"
            ));
        }
        let candidate = directory.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

async fn load_config_at_cancellable(
    source: &ConfigSource,
    path: Option<PathBuf>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<ConfigFile> {
    let Some(path) = path else {
        return Ok(ConfigFile::default());
    };
    let read_result =
        async_io::read_to_string_cancellable(&path, cancellation, async_io::MAX_READ_BYTES).await;
    config_from_read_result(source, &path, read_result)
}

async fn load_global_config_cancellable(
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Option<ConfigFile>> {
    let path = global_config_path()?;
    let read_result =
        async_io::read_to_string_cancellable(&path, cancellation, async_io::MAX_READ_BYTES).await;
    optional_config_from_read_result(&path, read_result)
}

/// Reports whether the optional global config exists without performing a
/// synchronous metadata call. `doctor` uses this to distinguish an absent
/// global file from a parsed empty config.
pub(crate) async fn global_config_exists_cancellable(
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<bool> {
    is_file_cancellable(&global_config_path()?, cancellation).await
}

async fn is_file_cancellable(
    path: &Path,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<bool> {
    let path = path.to_owned();
    async_io::run_blocking(
        move |cancelled| {
            use std::sync::atomic::Ordering;

            if cancelled.load(Ordering::Acquire) {
                bail!(crate::error::Interrupted::cancelled(
                    "configuration metadata lookup was cancelled"
                ));
            }
            Ok(path.is_file())
        },
        cancellation,
    )
    .await
}

/// Parses `contents` (already read from `path`) into a `ConfigFile` and
/// resolves its registry paths against `path`'s parent directory — the one
/// piece of post-processing both the project and the global config load
/// need, factored out so [`load_config_at`]/[`load_global_config`] share it.
fn parse_config_file(path: &Path, contents: &str) -> Result<ConfigFile> {
    let mut config: ConfigFile = serde_yaml::from_str(contents).with_context(|| {
        format!(
            "failed to parse YAML configuration file '{}'",
            path.display()
        )
    })?;
    if let Some(dir) = path.parent() {
        resolve_registry_paths_in_place(&mut config, dir);
    }
    Ok(config)
}

fn load_config_at(source: &ConfigSource, path: Option<PathBuf>) -> Result<ConfigFile> {
    let Some(path) = path else {
        return Ok(ConfigFile::default());
    };
    config_from_read_result(source, &path, async_io::read_to_string_sync(&path))
}

/// The non-I/O core [`load_config_at`]/[`load_config_at_cancellable`] share:
/// given a project config file's read attempt, either parse it, fall back to
/// `ConfigFile::default()` on a missing file (unless `source` names the path
/// explicitly — see the inline note below), or wrap the read error with
/// context. Factored out so the sync and cancellation-aware loaders can each
/// own just their own I/O call and still apply this decision identically —
/// see `async_io::read_to_string_sync`'s doc for why a `trait`-based
/// injection point for the read itself was rejected instead.
/// The read-failure context message shared by [`config_from_read_result`]/
/// [`optional_config_from_read_result`] — previously duplicated literally at
/// each site (see the design plan's B1 note); factored out so a future
/// wording change can't drift between the project and global loaders.
fn config_read_error_context(path: &Path) -> String {
    format!(
        "failed to read YAML configuration file '{}'",
        path.display()
    )
}

fn config_from_read_result(
    source: &ConfigSource,
    path: &Path,
    read_result: Result<String>,
) -> Result<ConfigFile> {
    match read_result {
        Ok(contents) => parse_config_file(path, &contents),
        // `Search` found nothing and falls back to defaults (unchanged
        // behavior); `Explicit` named this exact path, so a missing file
        // here falls through to the `with_context` error below instead —
        // the user asked for it by name, so silently using defaults would
        // hide a typo.
        Err(error)
            if !matches!(source, ConfigSource::Explicit(_)) && async_io::is_not_found(&error) =>
        {
            Ok(ConfigFile::default())
        }
        Err(error) => Err(error).with_context(|| config_read_error_context(path)),
    }
}

/// Loads the global config file at [`global_config_path`], or `None` when it
/// doesn't exist (not an error — the global file is optional). Unlike the
/// project file's [`load_config_at`], there is no `--no-config`/`Explicit`
/// case to special-case here: this is only ever called for
/// [`ConfigSource::Search`] (see [`load_config`]).
fn load_global_config() -> Result<Option<ConfigFile>> {
    let path = global_config_path()?;
    optional_config_from_read_result(&path, async_io::read_to_string_sync(&path))
}

/// The non-I/O core [`load_global_config`]/[`load_global_config_cancellable`]
/// share — see [`config_from_read_result`]'s doc for why this is split from
/// the I/O rather than injected as a trait. The global file has no
/// `Explicit`/`--no-config` case (see [`load_global_config`]'s doc), so a
/// missing file is always `Ok(None)` here, never the caller's choice.
fn optional_config_from_read_result(
    path: &Path,
    read_result: Result<String>,
) -> Result<Option<ConfigFile>> {
    match read_result {
        Ok(contents) => Ok(Some(parse_config_file(path, &contents)?)),
        Err(error) if async_io::is_not_found(&error) => Ok(None),
        Err(error) => Err(error).with_context(|| config_read_error_context(path)),
    }
}
