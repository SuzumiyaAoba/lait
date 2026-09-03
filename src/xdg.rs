//! XDG Base Directory resolution shared by `config::global_config_path`
//! (`$XDG_CONFIG_HOME`) and `history::history_path` (`$XDG_DATA_HOME`) — the
//! only two places lait reads a user-wide (not project-local) path. Both
//! follow the same rule: the named environment variable when it's set to a
//! non-blank value, else `$HOME`/`%USERPROFILE%` plus a fixed fallback
//! suffix (`.config`/`.local/share`, per the XDG Base Directory spec's
//! defaults).

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Resolves one XDG base directory: `$<env_var>` when set and non-blank,
/// else `$HOME`/`%USERPROFILE%` joined with `home_fallback_suffix`.
/// `purpose` names what the caller is trying to locate, for the error
/// message when neither `HOME` nor `USERPROFILE` is set.
pub(crate) fn base_dir(
    env_var: &str,
    home_fallback_suffix: &[&str],
    purpose: &str,
) -> Result<PathBuf> {
    if let Ok(dir) = std::env::var(env_var)
        && !dir.trim().is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .with_context(|| {
            format!(
                "failed to determine the home directory (HOME/USERPROFILE is not set) to locate \
                 {purpose}"
            )
        })?;
    let mut path = PathBuf::from(home);
    path.extend(home_fallback_suffix);
    Ok(path)
}
