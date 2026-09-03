//! Resolves an `api_key_cmd:` (see `config::CommandSpec`) by running the
//! configured command and using its (trimmed) stdout as the API key — the
//! standard way to keep a secret manager (1Password, pass, gopass, aws
//! secretsmanager, ...) out of the config file's plaintext, an alternative to
//! `${VAR_NAME}`'s pre-exported-environment-variable requirement (see
//! `AGENTS.md`'s Security and Configuration section).

use std::{
    collections::HashMap,
    process::{Command, Stdio},
    sync::Mutex,
};

use anyhow::{Context, Result, bail};

use crate::{config::CommandSpec, process};

/// Caches a resolved secret for the rest of the process's lifetime, keyed by
/// the command spec itself — a config resolved repeatedly (every workflow
/// step, every `for_each` iteration, every subagent call) must not
/// re-invoke a secrets-manager CLI on every single call, some of which
/// prompt for Touch ID/a passphrase each time. Never written to disk or
/// logged (`logging::mask_secret` masks it wherever it might otherwise be
/// traced).
static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Resolves `spec`, running its command once per distinct spec and caching
/// the result (see `CACHE`) for the rest of this process's lifetime.
/// Blocking: `resolve_endpoint`'s only two callers
/// (`resolve_request_settings`/`models::list_remote`) are both reached only
/// through the async command path (see `app::needs_async_runtime`), so this
/// always runs on a tokio worker thread — `tokio::task::block_in_place`
/// hands the blocking wait off that thread instead of stalling every other
/// task on it, which matters since a secrets-manager CLI can block for
/// seconds on a Touch ID/passphrase prompt.
pub(crate) fn resolve(spec: &CommandSpec) -> Result<String> {
    let key = cache_key(spec);
    if let Some(cached) = CACHE
        .lock()
        .expect("api_key_cmd cache lock poisoned")
        .get_or_insert_with(HashMap::new)
        .get(&key)
    {
        return Ok(cached.clone());
    }
    let secret = tokio::task::block_in_place(|| run(spec))?;
    CACHE
        .lock()
        .expect("api_key_cmd cache lock poisoned")
        .get_or_insert_with(HashMap::new)
        .insert(key, secret.clone());
    Ok(secret)
}

/// A cache key that never collides a `Shell` spec with an `Argv` one that
/// happens to render the same text (e.g. `Shell("op read x")` vs.
/// `Argv(["op read x"])`, a single-element list — the two run very
/// differently, one through a shell and one execed directly).
fn cache_key(spec: &CommandSpec) -> String {
    match spec {
        CommandSpec::Shell(command) => format!("sh:{command}"),
        CommandSpec::Argv(argv) => format!("argv:{argv:?}"),
    }
}

/// Runs `spec` once: a `Shell` string through `sh -c` (`cmd /C` on Windows,
/// so pipes/quoting/subshells work the way the issue's own example,
/// `op read op://Personal/OpenAI/api-key`, expects), an `Argv` list execed
/// directly with no shell involved. Either way, stdin is closed (`Stdio::null`)
/// so a command that unexpectedly tries to read from it fails fast rather
/// than hanging the whole request.
fn run(spec: &CommandSpec) -> Result<String> {
    let mut command = match spec {
        CommandSpec::Shell(script) => {
            let mut command = shell_command();
            command.arg(script);
            command
        }
        CommandSpec::Argv(argv) => {
            let [program, args @ ..] = argv.as_slice() else {
                bail!("api_key_cmd's command list must not be empty");
            };
            let mut command = Command::new(program);
            command.args(args);
            command
        }
    };
    command.stdin(Stdio::null());

    let output = command
        .output()
        .with_context(|| format!("failed to run api_key_cmd {spec:?}"))?;
    if !output.status.success() {
        bail!(
            "api_key_cmd exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout =
        String::from_utf8(output.stdout).context("api_key_cmd's output was not valid UTF-8")?;
    let key = process::strip_one_trailing_line_ending(stdout);
    if key.is_empty() {
        bail!("api_key_cmd produced no output");
    }
    Ok(key)
}

#[cfg(windows)]
fn shell_command() -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C");
    command
}

#[cfg(not(windows))]
fn shell_command() -> Command {
    let mut command = Command::new("sh");
    command.arg("-c");
    command
}
