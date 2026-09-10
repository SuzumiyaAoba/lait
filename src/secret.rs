//! Resolves `api_key_cmd:` values (see `config::CommandSpec`) at the request
//! boundary. Configuration parsing and endpoint selection retain command
//! specs as inert data; this module is the only place that executes one.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::sync::Mutex;

use crate::{
    config::{ApiKeySource, CommandSpec},
    process,
};

/// A secret-manager command must not be able to hold a request forever or
/// consume unbounded memory. These defaults intentionally live here rather
/// than in the YAML schema: they are execution-safety limits, not user
/// credentials or endpoint settings.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// One cached command spec's resolved value, or `None` while unresolved
/// (never yet run, or the last run failed/was cancelled). The `Mutex`
/// doubles as the per-spec serialization point `resolve` locks while running
/// the command, so concurrent resolutions of the same spec share one
/// in-flight run instead of executing it twice.
type SecretCell = Arc<Mutex<Option<String>>>;

/// Per-application secret resolver. Successful values are cached by their
/// exact command spec, while an in-flight command is serialized behind a
/// per-spec mutex. A failed or cancelled command leaves the cell empty, so a
/// later request can retry it instead of inheriting a permanently cached
/// failure.
#[derive(Clone)]
pub(crate) struct SecretResolver {
    cache: Arc<Mutex<HashMap<CommandSpec, SecretCell>>>,
    timeout: Duration,
    max_output_bytes: usize,
}

impl Default for SecretResolver {
    fn default() -> Self {
        Self::with_limits(DEFAULT_TIMEOUT, DEFAULT_MAX_OUTPUT_BYTES)
    }
}

impl SecretResolver {
    /// Builds a resolver with the crate-wide default timeout/output-size
    /// limits (`DEFAULT_TIMEOUT`/`DEFAULT_MAX_OUTPUT_BYTES`) — what every
    /// production call site uses; `with_limits` exists so tests can shrink
    /// both.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Builds a resolver with explicit limits, bypassing the crate-wide
    /// defaults `new` uses.
    pub(crate) fn with_limits(timeout: Duration, max_output_bytes: usize) -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            timeout,
            max_output_bytes,
        }
    }

    /// Resolves an endpoint's selected API-key source. Literals are returned
    /// without spawning a process; [`ApiKeySource::Command`] is executed only
    /// here, immediately before its request is issued.
    pub(crate) async fn resolve(
        &self,
        source: &ApiKeySource,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Option<String>> {
        ensure_not_cancelled(cancellation.as_ref())?;
        match source {
            ApiKeySource::Absent => Ok(None),
            ApiKeySource::Literal(value) => Ok(Some(value.clone())),
            ApiKeySource::Command(spec) => self.resolve_command(spec, cancellation).await.map(Some),
        }
    }

    async fn resolve_command(
        &self,
        spec: &CommandSpec,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<String> {
        ensure_not_cancelled(cancellation.as_ref())?;
        let cell = {
            let mut cache = self.cache.lock().await;
            cache
                .entry(spec.clone())
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };

        let mut value = if let Some(cancellation) = &cancellation {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(cancellation_error());
                }
                value = cell.lock() => value,
            }
        } else {
            cell.lock().await
        };
        if let Some(secret) = value.as_ref() {
            return Ok(secret.clone());
        }

        let secret = self.run(spec, cancellation.clone()).await?;
        ensure_not_cancelled(cancellation.as_ref())?;
        *value = Some(secret.clone());
        Ok(secret)
    }

    async fn run(
        &self,
        spec: &CommandSpec,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<String> {
        let argv = command_argv(spec)?;
        let output =
            process::run_bounded_command(&argv, self.timeout, self.max_output_bytes, cancellation)
                .await?;
        if !output.status.success() {
            // Do not include stderr: secret-manager CLIs frequently echo
            // account names, paths, or even the secret itself in diagnostics.
            bail!("api_key_cmd exited with {}", output.status);
        }
        let stdout =
            String::from_utf8(output.stdout).context("api_key_cmd's output was not valid UTF-8")?;
        let secret = process::strip_one_trailing_line_ending(stdout);
        if secret.is_empty() {
            bail!("api_key_cmd produced no output");
        }
        Ok(secret)
    }
}

fn cancellation_error() -> anyhow::Error {
    crate::error::Interrupted::cancelled("api_key_cmd resolution was cancelled").into()
}

fn ensure_not_cancelled(cancellation: Option<&tokio_util::sync::CancellationToken>) -> Result<()> {
    if cancellation.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        return Err(cancellation_error());
    }
    Ok(())
}

fn command_argv(spec: &CommandSpec) -> Result<Vec<String>> {
    match spec {
        CommandSpec::Shell(script) => Ok(shell_command(script)),
        CommandSpec::Argv(argv) if argv.is_empty() => {
            bail!("api_key_cmd's command list must not be empty")
        }
        CommandSpec::Argv(argv) => Ok(argv.clone()),
    }
}

#[cfg(windows)]
fn shell_command(script: &str) -> Vec<String> {
    vec!["cmd".to_owned(), "/C".to_owned(), script.to_owned()]
}

#[cfg(not(windows))]
fn shell_command(script: &str) -> Vec<String> {
    vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_literal_and_command_sources() {
        let resolver = SecretResolver::default();
        assert_eq!(
            resolver.resolve(&ApiKeySource::Absent, None).await.unwrap(),
            None
        );
        assert_eq!(
            resolver
                .resolve(&ApiKeySource::Literal("literal".to_owned()), None)
                .await
                .unwrap()
                .as_deref(),
            Some("literal")
        );
        let source = ApiKeySource::Command(CommandSpec::Shell("printf command-secret".to_owned()));
        assert_eq!(
            resolver.resolve(&source, None).await.unwrap().as_deref(),
            Some("command-secret")
        );
    }

    #[tokio::test]
    async fn caches_success_but_retries_after_a_failure() {
        let marker = std::env::temp_dir().join(format!(
            "lait-secret-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script = format!(
            "if [ -e '{}' ]; then printf recovered; else touch '{}'; exit 1; fi",
            marker.display(),
            marker.display()
        );
        let source = ApiKeySource::Command(CommandSpec::Shell(script));
        let resolver = SecretResolver::default();
        assert!(resolver.resolve(&source, None).await.is_err());
        assert_eq!(
            resolver.resolve(&source, None).await.unwrap().as_deref(),
            Some("recovered")
        );
        assert_eq!(
            resolver.resolve(&source, None).await.unwrap().as_deref(),
            Some("recovered")
        );
        std::fs::remove_file(marker).ok();
    }

    #[tokio::test]
    async fn enforces_output_limit_without_leaking_stderr() {
        let resolver = SecretResolver::with_limits(Duration::from_secs(5), 8);
        let too_large =
            ApiKeySource::Command(CommandSpec::Shell("printf 123456789; sleep 30".to_owned()));
        let error = resolver.resolve(&too_large, None).await.unwrap_err();
        let error_text = format!("{error:#}");
        assert!(error_text.contains("limit of 8 bytes"), "{error_text}");

        let failed = ApiKeySource::Command(CommandSpec::Shell(
            "printf hidden-secret >&2; exit 7".to_owned(),
        ));
        let error = resolver.resolve(&failed, None).await.unwrap_err();
        assert!(!error.to_string().contains("hidden-secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancels_a_running_command_and_reaps_its_process_tree() {
        let marker = std::env::temp_dir().join(format!(
            "lait-secret-cancelled-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let child_pid_file = marker.with_extension("pid");
        let source = ApiKeySource::Command(CommandSpec::Shell(format!(
            "touch '{}'; (sleep 30) & echo $! > '{}'; wait",
            marker.display(),
            child_pid_file.display()
        )));
        let resolver = SecretResolver::default();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn({
            let cancellation = cancellation.clone();
            async move { resolver.resolve(&source, Some(cancellation)).await }
        });
        for _ in 0..100 {
            if marker.exists() && child_pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(marker.exists(), "the command did not start");
        let child_pid = std::fs::read_to_string(&child_pid_file)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("cancelling a running command must return promptly")
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::error::Interrupted>()),
            "{error:#}"
        );

        for _ in 0..100 {
            // `kill(pid, 0)` only probes existence and does not signal the
            // process. The process group cleanup should make the background
            // child disappear along with the command shell.
            let exists = unsafe { libc::kill(child_pid, 0) == 0 };
            if !exists {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!unsafe { libc::kill(child_pid, 0) == 0 });
        std::fs::remove_file(marker).ok();
        std::fs::remove_file(child_pid_file).ok();
    }

    #[tokio::test]
    async fn pre_cancelled_sources_are_rejected_without_running_a_command() {
        let marker = std::env::temp_dir().join(format!(
            "lait-secret-pre-cancelled-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = ApiKeySource::Command(CommandSpec::Shell(format!(
            "touch '{}'; printf should-not-run",
            marker.display()
        )));
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let resolver = SecretResolver::default();

        let error = resolver
            .resolve(&source, Some(cancellation.clone()))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<crate::error::Interrupted>().is_some());
        assert!(!marker.exists(), "a pre-cancelled command must not run");

        let error = resolver
            .resolve(
                &ApiKeySource::Literal("cached-value".to_owned()),
                Some(cancellation.clone()),
            )
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<crate::error::Interrupted>().is_some());

        let command = ApiKeySource::Command(CommandSpec::Shell("printf cached-value".to_owned()));
        assert_eq!(
            resolver.resolve(&command, None).await.unwrap().as_deref(),
            Some("cached-value")
        );
        let error = resolver
            .resolve(&command, Some(cancellation))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<crate::error::Interrupted>().is_some());
        std::fs::remove_file(marker).ok();
    }

    #[tokio::test]
    async fn a_cancelled_waiter_does_not_stop_the_initializer() {
        let marker = std::env::temp_dir().join(format!(
            "lait-secret-waiter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = ApiKeySource::Command(CommandSpec::Shell(format!(
            "touch '{}'; sleep 1; printf initializer-value",
            marker.display()
        )));
        let resolver = Arc::new(SecretResolver::default());
        let initializer = tokio::spawn({
            let resolver = Arc::clone(&resolver);
            let source = source.clone();
            async move { resolver.resolve(&source, None).await }
        });
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(marker.exists(), "initializer command did not start");

        let cancellation = tokio_util::sync::CancellationToken::new();
        let waiter = tokio::spawn({
            let resolver = Arc::clone(&resolver);
            let source = source.clone();
            let cancellation = cancellation.clone();
            async move { resolver.resolve(&source, Some(cancellation)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let cancelled_at = std::time::Instant::now();
        cancellation.cancel();
        let waiter_error = tokio::time::timeout(Duration::from_millis(500), waiter)
            .await
            .expect("a cancelled waiter must return promptly")
            .unwrap()
            .unwrap_err();
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(500),
            "waiter cancellation was not prompt"
        );
        assert!(
            waiter_error
                .downcast_ref::<crate::error::Interrupted>()
                .is_some()
        );

        let value = tokio::time::timeout(Duration::from_secs(3), initializer)
            .await
            .expect("initializer must continue after waiter cancellation")
            .unwrap()
            .unwrap();
        assert_eq!(value.as_deref(), Some("initializer-value"));
        std::fs::remove_file(marker).ok();
    }
}
