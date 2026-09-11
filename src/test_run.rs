//! `lait test`: runs test definition YAML files (a target workflow, an
//! input/vars, a `--record`ed replay cassette directory, and `assert:`
//! assertions), without replaying LLM API requests, reporting pass/fail per
//! file. Workflow-side tools and other I/O retain their configured behavior.
//! See docs/usage/ja/testing.md.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;

use crate::{
    assert::{self, Assertion},
    cli::{TestArgs, TestFormat},
    config::{self, ConfigFile, ConfigSource},
    engine::{AppServices, RunContext},
    signal, storage,
    workflow::{
        self, WorkflowScope,
        exec::{RunStepsFrame, run_steps},
    },
};

/// Upper bound on concurrently in-flight test files — matches
/// `engine::tool_loop::MAX_CONCURRENT_TOOL_CALLS`'s rationale: independent
/// workflow runs, bounded so a large test suite doesn't open unbounded
/// concurrent connections to a replay directory or configured endpoint.
const TEST_CONCURRENCY: usize = 8;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestDefinition {
    /// Path to the target workflow file, relative to this test definition
    /// file's own directory.
    workflow: PathBuf,
    /// The initial input passed to the workflow's first step. Defaults to an
    /// empty string when omitted (a workflow whose steps never reference
    /// `{{ input }}` has no need for one).
    #[serde(default)]
    input: String,
    /// `{{ vars.<key> }}` overrides, in the same shape `--var` ultimately
    /// builds — written directly as typed YAML here rather than
    /// `KEY=VALUE` strings, since there is no shell to parse them from.
    #[serde(default)]
    vars: serde_json::Map<String, serde_json::Value>,
    /// Path to a directory previously produced by `lait run --record`,
    /// relative to this test definition file's own directory. Every request
    /// the workflow makes is answered from here; one with no matching
    /// cassette fails the test (see `crate::cassette::load`).
    replay: PathBuf,
    #[serde(default)]
    assert: Vec<Assertion>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TestStatus {
    Pass,
    Fail,
}

struct TestOutcome {
    file: PathBuf,
    status: TestStatus,
    /// Human-readable failure reasons: either a single "could not load/run
    /// this test" entry, or one entry per failed `assert:` item.
    failures: Vec<String>,
}

/// Collects test definitions from explicit files and directories.
///
/// The discovery policy is deliberately stricter than the later file read:
/// explicit targets must be regular files or directories, while a directory
/// walk only includes regular `.yml`/`.yaml` files. Symbolic links and special
/// files are never followed. A symlink or special file encountered below an
/// explicit directory is skipped, whereas passing one explicitly is an error
/// so a typo cannot silently result in zero tests. Canonical file/directory
/// identities prevent overlapping targets from producing duplicate work, and
/// the returned paths are sorted for stable reports.
/// Cancellation-aware target discovery for the async `lait test` entry point.
/// Directory traversal and canonicalization perform blocking metadata I/O, so
/// keep the entire collector on the bounded filesystem worker.
async fn expand_test_targets_cancellable(
    paths: &[PathBuf],
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Vec<PathBuf>> {
    let paths = paths.to_owned();
    crate::async_io::run_blocking(
        move |cancelled| expand_test_targets_with_cancellation(&paths, Some(cancelled)),
        cancellation,
    )
    .await
}

fn expand_test_targets_with_cancellation(
    paths: &[PathBuf],
    cancellation: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Vec<PathBuf>> {
    let mut collector = TestTargetCollector::default();
    for path in paths {
        collector.collect_explicit(path, cancellation)?;
    }
    collector.files.sort();
    Ok(collector.files)
}

#[derive(Default)]
struct TestTargetCollector {
    files: Vec<PathBuf>,
    seen_files: HashSet<PathBuf>,
    visited_directories: HashSet<PathBuf>,
}

impl TestTargetCollector {
    fn collect_explicit(
        &mut self,
        path: &Path,
        cancellation: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        check_discovery_cancellation(cancellation)?;
        let file_type = std::fs::symlink_metadata(path)
            .with_context(|| format!("failed to read test target '{}'", path.display()))?
            .file_type();
        if file_type.is_symlink() {
            bail!(
                "test target '{}' is a symbolic link; pass a regular file or directory",
                path.display()
            );
        }
        if file_type.is_file() {
            self.add_file(path, cancellation)?;
            return Ok(());
        }
        if file_type.is_dir() {
            self.collect_directory(path, cancellation)
        } else {
            bail!(
                "test target '{}' is not a regular file or directory",
                path.display()
            );
        }
    }

    fn collect_directory(
        &mut self,
        path: &Path,
        cancellation: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        check_discovery_cancellation(cancellation)?;
        let identity = std::fs::canonicalize(path)
            .with_context(|| format!("failed to resolve test directory '{}'", path.display()))?;
        if !self.visited_directories.insert(identity) {
            return Ok(());
        }

        let mut entries: Vec<_> = std::fs::read_dir(path)
            .with_context(|| storage::read_dir_context(path))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| storage::read_dir_context(path))?;
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            check_discovery_cancellation(cancellation)?;
            let entry_path = entry.path();
            let file_name = entry_path.file_name().and_then(|name| name.to_str());
            if file_name.is_some_and(|name| name.starts_with('.')) {
                continue;
            }

            // `DirEntry::file_type` and `symlink_metadata` inspect the entry
            // itself, so this branch cannot accidentally turn a symlink to a
            // directory into a recursive walk.
            let file_type = entry.file_type().with_context(|| {
                format!("failed to inspect test path '{}'", entry_path.display())
            })?;
            if file_type.is_symlink() || (!file_type.is_file() && !file_type.is_dir()) {
                continue;
            }
            if file_type.is_dir() {
                self.collect_directory(&entry_path, cancellation)?;
                continue;
            }
            if matches!(
                entry_path.extension().and_then(|ext| ext.to_str()),
                Some("yml") | Some("yaml")
            ) {
                self.add_file(&entry_path, cancellation)?;
            }
        }
        Ok(())
    }

    fn add_file(
        &mut self,
        path: &Path,
        cancellation: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        check_discovery_cancellation(cancellation)?;
        let identity = std::fs::canonicalize(path)
            .with_context(|| format!("failed to resolve test file '{}'", path.display()))?;
        check_discovery_cancellation(cancellation)?;
        if self.seen_files.insert(identity) {
            self.files.push(path.to_path_buf());
        }
        Ok(())
    }
}

fn check_discovery_cancellation(
    cancellation: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    if cancellation.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
        bail!(crate::error::Interrupted::cancelled(
            "test target discovery was cancelled"
        ));
    }
    Ok(())
}

/// Runs one test definition file, never propagating an error: a load/parse
/// failure, a missing workflow/replay path, or a workflow execution failure
/// (most notably an unrecorded request — see `crate::cassette::load`) are all
/// reported as this file's single failure reason instead, so one bad test
/// file doesn't stop the rest from running (the same policy `lint::run`
/// applies across its own files).
async fn run_test_file(
    path: &Path,
    file_config: &Arc<ConfigFile>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<TestOutcome> {
    let root_cancel = cancel.clone();
    match run_test_file_inner(path, file_config, cancel).await {
        Ok(failures) => Ok(TestOutcome {
            file: path.to_path_buf(),
            status: if failures.is_empty() {
                TestStatus::Pass
            } else {
                TestStatus::Fail
            },
            failures,
        }),
        Err(error) if cancel_is_active(&error, &root_cancel) => Err(error),
        Err(error) => Ok(TestOutcome {
            file: path.to_path_buf(),
            status: TestStatus::Fail,
            failures: vec![format!("{error:#}")],
        }),
    }
}

fn cancel_is_active(
    error: &anyhow::Error,
    cancellation: &tokio_util::sync::CancellationToken,
) -> bool {
    cancellation.is_cancelled()
        && error
            .chain()
            .any(|cause| cause.is::<crate::error::Interrupted>())
}

async fn run_test_file_inner(
    path: &Path,
    file_config: &Arc<ConfigFile>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<Vec<String>> {
    let contents = crate::async_io::read_to_string_cancellable(
        path,
        Some(cancel.clone()),
        crate::async_io::MAX_READ_BYTES,
    )
    .await
    .with_context(|| format!("failed to read test definition '{}'", path.display()))?;
    let definition: TestDefinition = serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse test definition '{}'", path.display()))?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let workflow_path = base_dir.join(&definition.workflow);
    let replay_dir = base_dir.join(&definition.replay);

    let mut wf = workflow::load_workflow_cancellable(&workflow_path, Some(cancel.clone())).await?;
    let scope = WorkflowScope::top_level(&mut wf, &workflow_path, Some(cancel.clone())).await?;

    let services = Arc::new(AppServices::new(Arc::clone(file_config)));
    let env = RunContext::new(Arc::clone(&services), cancel)
        .with_vars(definition.vars)
        .with_record_replay(None, Some(replay_dir))?;

    let outcome = services
        .finish(run_steps(
            &wf.steps,
            definition.input,
            workflow::StepOutputs::new(),
            RunStepsFrame {
                scope: &scope,
                env: &env,
                start_counter: 0,
                progress_prefix: "",
                cancellation: Some(env.root_token()),
                placement: Default::default(),
            },
        ))
        .await
        .with_context(|| format!("workflow '{}'", workflow_path.display()))?;

    let failures = assert::evaluate(
        &definition.assert,
        None,
        &outcome.output,
        Some(env.operation_token()),
    )
    .await;
    Ok(failures
        .into_iter()
        .map(|failure| format!("assertion {}: {}", failure.position, failure.message))
        .collect())
}

fn print_text_report(outcomes: &[TestOutcome]) {
    let mut failed = 0usize;
    for outcome in outcomes {
        match outcome.status {
            TestStatus::Pass => println!("{}: PASS", outcome.file.display()),
            TestStatus::Fail => {
                failed += 1;
                println!("{}: FAIL", outcome.file.display());
                for reason in &outcome.failures {
                    println!("  {reason}");
                }
            }
        }
    }
    println!(
        "{} passed, {} failed, {} total",
        outcomes.len() - failed,
        failed,
        outcomes.len()
    );
}

fn print_json_report(outcomes: &[TestOutcome]) -> Result<()> {
    let results: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|outcome| {
            serde_json::json!({
                "file": outcome.file.display().to_string(),
                "status": match outcome.status {
                    TestStatus::Pass => "pass",
                    TestStatus::Fail => "fail",
                },
                "failures": outcome.failures,
            })
        })
        .collect();
    println!("{}", serde_json::to_string(&results)?);
    Ok(())
}

pub(crate) async fn run(
    args: TestArgs,
    config_source: ConfigSource,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    signal::spawn_handler(cancel.clone());
    let targets = expand_test_targets_cancellable(&args.paths, Some(cancel.clone())).await?;
    let file_config =
        Arc::new(config::load_config_cancellable(&config_source, Some(cancel.clone())).await?);

    // Each test file runs a whole workflow (potentially several model
    // calls) independently of every other, so running them one at a time
    // made a directory of N files cost N times a single file's wall clock.
    // `run_test_file` already turns an ordinary test failure into an
    // `Ok(TestOutcome { status: Fail, .. })` — only a genuine cancellation
    // (SIGINT, a workflow deadline) surfaces as `Err` here — so
    // `try_collect` short-circuits on cancellation exactly as the previous
    // sequential `?` did, while every non-cancelled file still runs to
    // completion and is reported, same as before.
    let run_futures = targets
        .iter()
        .map(|target| run_test_file(target, &file_config, cancel.clone()));
    let outcomes: Vec<TestOutcome> = futures_util::stream::iter(run_futures)
        .buffered(TEST_CONCURRENCY)
        .try_collect()
        .await?;

    match args.format {
        TestFormat::Text => print_text_report(&outcomes),
        TestFormat::Json => print_json_report(&outcomes)?,
    }

    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.status == TestStatus::Fail)
        .count();
    if failed > 0 {
        bail!("{failed} of {} test file(s) failed", outcomes.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::expand_test_targets_with_cancellation;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lait-test-run-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn expands_a_directory_recursively_and_sorts_yaml_files() {
        let dir = temp_dir("recurse");
        std::fs::write(dir.join("b.yml"), "").unwrap();
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested").join("a.yaml"), "").unwrap();
        std::fs::write(dir.join("ignored.txt"), "").unwrap();

        let files =
            expand_test_targets_with_cancellation(std::slice::from_ref(&dir), None).unwrap();
        assert_eq!(files, vec![dir.join("b.yml"), dir.join("nested/a.yaml")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skips_dotfiles_and_dot_directories() {
        let dir = temp_dir("dotfiles");
        std::fs::write(dir.join("visible.yml"), "").unwrap();
        std::fs::write(dir.join(".hidden.yml"), "").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("config.yml"), "").unwrap();

        let files =
            expand_test_targets_with_cancellation(std::slice::from_ref(&dir), None).unwrap();
        assert_eq!(files, vec![dir.join("visible.yml")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deduplicates_overlapping_explicit_and_directory_targets() {
        let dir = temp_dir("deduplicate");
        let file = dir.join("case.yml");
        std::fs::write(&file, "").unwrap();

        let files =
            expand_test_targets_with_cancellation(&[file.clone(), dir.clone()], None).unwrap();
        assert_eq!(files, vec![file]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinked_files_and_directories_without_following_cycles() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlinks");
        let nested = dir.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("case.yml");
        std::fs::write(&file, "").unwrap();

        symlink(&dir, nested.join("cycle")).unwrap();
        symlink(&file, dir.join("alias.yml")).unwrap();

        let files =
            expand_test_targets_with_cancellation(std::slice::from_ref(&dir), None).unwrap();
        assert_eq!(files, vec![file]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_an_explicit_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("explicit-symlink");
        let file = dir.join("case.yml");
        let link = dir.join("link.yml");
        std::fs::write(&file, "").unwrap();
        symlink(&file, &link).unwrap();

        let error =
            expand_test_targets_with_cancellation(std::slice::from_ref(&link), None).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn skips_special_files_during_directory_discovery() {
        use std::{
            path::PathBuf,
            time::{SystemTime, UNIX_EPOCH},
        };

        // Keep this fixture under /tmp so a platform-specific special-file
        // path limit cannot interfere with the discovery assertion.
        let dir = PathBuf::from("/tmp").join(format!(
            "lait-test-special-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let fifo_path = dir.join("ignored.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .unwrap();
        assert!(status.success());
        let files =
            expand_test_targets_with_cancellation(std::slice::from_ref(&dir), None).unwrap();
        assert!(files.is_empty());
        std::fs::remove_file(&fifo_path).ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}
