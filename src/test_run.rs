//! `lait test`: runs test definition YAML files (a target workflow, an
//! input/vars, a `--record`ed replay cassette directory, and `assert:`
//! assertions) with no network access at all, reporting pass/fail per file.
//! See docs/usage/ja/testing.md.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    assert::{self, Assertion},
    cli::{TestArgs, TestFormat},
    config::{self, ConfigFile, ConfigSource},
    engine::AppContext,
    signal,
    workflow::{
        self, WorkflowScope,
        exec::{RunStepsFrame, run_steps},
    },
};

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

/// Recursively collects `.yml`/`.yaml` files from `paths` (files are taken
/// as-is; directories are searched recursively), sorted for stable output —
/// the same directory-expansion shape `lint::run` uses for its own targets.
fn expand_test_targets(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        collect_test_files(path, &mut files)?;
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_test_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    if metadata.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(path)
        .with_context(|| format!("failed to read directory '{}'", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read directory '{}'", path.display()))?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let entry_path = entry.path();
        let file_name = entry_path.file_name().and_then(|name| name.to_str());
        if file_name.is_some_and(|name| name.starts_with('.')) {
            continue;
        }
        if entry_path.is_dir() {
            collect_test_files(&entry_path, files)?;
            continue;
        }
        if matches!(
            entry_path.extension().and_then(|ext| ext.to_str()),
            Some("yml") | Some("yaml")
        ) {
            files.push(entry_path);
        }
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
) -> TestOutcome {
    match run_test_file_inner(path, file_config, cancel).await {
        Ok(failures) => TestOutcome {
            file: path.to_path_buf(),
            status: if failures.is_empty() {
                TestStatus::Pass
            } else {
                TestStatus::Fail
            },
            failures,
        },
        Err(error) => TestOutcome {
            file: path.to_path_buf(),
            status: TestStatus::Fail,
            failures: vec![format!("{error:#}")],
        },
    }
}

async fn run_test_file_inner(
    path: &Path,
    file_config: &Arc<ConfigFile>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<Vec<String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read test definition '{}'", path.display()))?;
    let definition: TestDefinition = serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse test definition '{}'", path.display()))?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let workflow_path = base_dir.join(&definition.workflow);
    let replay_dir = base_dir.join(&definition.replay);

    let mut wf = workflow::load_workflow(&workflow_path)?;
    let scope = WorkflowScope::top_level(&mut wf, &workflow_path)?;

    let env = AppContext::new(Arc::clone(file_config))
        .with_vars(definition.vars)
        .with_cancel(cancel)
        .with_record_replay(None, Some(replay_dir));

    let outcome = env
        .finish(run_steps(
            &wf.steps,
            definition.input,
            workflow::StepOutputs::new(),
            RunStepsFrame {
                scope: &scope,
                env: &env,
                start_counter: 0,
                progress_prefix: "",
                cancellation: env.cancel.clone(),
            },
        ))
        .await
        .with_context(|| format!("workflow '{}'", workflow_path.display()))?;

    let failures = assert::evaluate(
        &definition.assert,
        None,
        &outcome.output,
        None,
        env.cancel.clone(),
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
    let targets = expand_test_targets(&args.paths)?;
    let file_config = Arc::new(config::load_config(&config_source)?);

    let mut outcomes = Vec::with_capacity(targets.len());
    for target in &targets {
        outcomes.push(run_test_file(target, &file_config, cancel.clone()).await);
    }

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
    use super::expand_test_targets;
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

        let files = expand_test_targets(std::slice::from_ref(&dir)).unwrap();
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

        let files = expand_test_targets(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(files, vec![dir.join("visible.yml")]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
