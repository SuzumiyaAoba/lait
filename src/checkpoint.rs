//! Workflow run checkpoints (`lait run --checkpoint`/`--resume`, `lait runs
//! list`/`show`): a snapshot of a run's state written to
//! `.lait/runs/<run-id>.json` after every top-level step completes, letting
//! `lait run <FILE> --resume <RUN_ID>` continue a failed run from its last
//! completed top-level step instead of re-running the whole workflow from
//! scratch. See `app::run_workflow` (the only writer/reader of these besides
//! `run` below) and docs/usage/ja/workflow.md.
//!
//! Resume only ever replays *top-level* steps: a router step (`switch`/
//! `parallel`/`loop`/`for_each`) always re-runs from its own beginning on
//! resume, since a checkpoint only records state between top-level steps,
//! never mid-router. A side-effecting node (`command`, `write_file`) inside
//! a router can therefore execute twice across a resume — a documented
//! consequence of this scope, not a bug.
//!
//! Unlike `session`/`history` (append-only JSONL logs, see `jsonl.rs`), a
//! checkpoint file is replaced wholesale on every step rather than appended
//! to, so it needs a whole-file write rather than a line append —
//! `jsonl.rs`'s primitives are all append-shaped and deliberately not
//! extended for this (see its own module doc's reasoning for not
//! generalizing further). A checkpoint write instead goes through a plain
//! temp-file-then-`rename` (atomic on the same filesystem) below; existence
//! checks and directory listing still reuse `jsonl`'s symlink-safe
//! relative-path primitives, since those shapes already fit.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    cli::{RunsAction, RunsCommand},
    jsonl, session, workflow,
};

/// The directory every checkpoint file lives under, relative to the current
/// directory — a project-local concept, like `session::SESSIONS_DIR`.
const RUNS_DIR: &str = ".lait/runs";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RunStatus {
    /// The run has not finished yet: either still in progress, or it failed
    /// after the last recorded step. `--resume` refuses any status but this.
    Failed,
    /// The run finished successfully. Kept on disk (rather than deleted) so
    /// `lait runs list`/`show` can still report it; `--resume` errors
    /// clearly on it instead of silently no-oping.
    Completed,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Failed => "failed",
            RunStatus::Completed => "completed",
        }
    }
}

/// One run's recorded state, written after every top-level step and read
/// back by `--resume`/`lait runs show`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub(crate) run_id: String,
    /// The resolved workflow file path (`resolve_run_target`'s output,
    /// rendered with `Path::display`), compared against a `--resume`
    /// invocation's own resolved FILE so a checkpoint can't be replayed
    /// against a different workflow file by mistake.
    pub(crate) workflow_path: String,
    pub(crate) initial_prompt: String,
    /// `lait run --var` overrides in effect for this run (see
    /// `workflow::build_vars`) — recorded so a resumed run's remaining steps
    /// still see `{{ vars.* }}`/`$vars` without the user having to repeat
    /// every `--var`. This means a checkpoint file may contain sensitive
    /// `--var` values verbatim, the same way it already contains the
    /// workflow's own input/intermediate output.
    #[serde(default)]
    pub(crate) vars: serde_json::Map<String, serde_json::Value>,
    /// Every top-level step's label, by position — see
    /// `app::top_level_step_labels` for how this differs from a step's
    /// runtime progress label. Used only to detect whether the workflow's
    /// step sequence changed since this checkpoint was written.
    pub(crate) top_level_labels: Vec<String>,
    /// How many top-level steps have completed (0 before the first one
    /// finishes). `--resume` continues from `wf.steps[completed_index..]`.
    pub(crate) completed_index: usize,
    pub(crate) counter: usize,
    pub(crate) current_input: String,
    pub(crate) steps_outputs: workflow::StepOutputs,
    pub(crate) status: RunStatus,
}

fn run_path(run_id: &str) -> Result<PathBuf> {
    // Run ids are generated internally (`generate_run_id`), but a
    // user-supplied `--resume <RUN_ID>`/`lait runs show <RUN_ID>` reaches
    // this the same way a `--session <NAME>` does — reuse its exact
    // path-traversal guard rather than duplicating it.
    session::validate_name(run_id)?;
    Ok(Path::new(RUNS_DIR).join(format!("{run_id}.json")))
}

/// A run id that sorts chronologically by creation time and satisfies
/// `session::validate_name`'s charset — an RFC 3339 timestamp's `:` would
/// not, so this rolls its own compact format instead:
/// `YYYYMMDD-HHMMSS-<4 hex digits from the current nanosecond>`. The hex
/// suffix only guards against two runs starting in the same second; it is
/// not a real uniqueness guarantee, which matters little for a
/// human-inspected filename.
pub(crate) fn generate_run_id() -> String {
    let now = chrono::Utc::now();
    format!(
        "{}-{:04x}",
        now.format("%Y%m%d-%H%M%S"),
        now.timestamp_subsec_nanos() % 0x1_0000
    )
}

/// Writes `checkpoint` to its run file, atomically: the full JSON body is
/// written to a temp file in the same directory, then renamed into place —
/// so a crash mid-write, or a concurrent `lait runs show`, never observes a
/// half-written file.
pub(crate) fn save(checkpoint: &Checkpoint) -> Result<()> {
    let path = run_path(&checkpoint.run_id)?;
    let dir = path
        .parent()
        .expect("run_path always returns a path under RUNS_DIR");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create directory '{}'", dir.display()))?;
    let body =
        serde_json::to_string_pretty(checkpoint).context("failed to serialize checkpoint")?;
    let tmp_path = Path::new(RUNS_DIR).join(format!("{}.json.tmp", checkpoint.run_id));
    std::fs::write(&tmp_path, body)
        .with_context(|| format!("failed to write '{}'", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("failed to save checkpoint to '{}'", path.display()))?;
    Ok(())
}

fn read(path: &Path) -> Result<Checkpoint> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&body)
        .with_context(|| format!("failed to parse checkpoint file '{}'", path.display()))
}

/// Loads run `run_id`'s checkpoint, failing with a clear error when it
/// doesn't exist.
pub(crate) fn load(run_id: &str) -> Result<Checkpoint> {
    let path = run_path(run_id)?;
    if !jsonl::path_exists(&path)? {
        bail!("no such checkpointed run '{run_id}'");
    }
    read(&path)
}

/// Lists every checkpointed run under `RUNS_DIR`, sorted by run id (which
/// sorts chronologically — see `generate_run_id`). Returns an empty `Vec`
/// when the directory doesn't exist yet.
pub(crate) fn list() -> Result<Vec<Checkpoint>> {
    let dir = Path::new(RUNS_DIR);
    if !jsonl::directory_exists(dir)? {
        return Ok(Vec::new());
    }
    let mut checkpoints = Vec::new();
    for entry in jsonl::read_dir(dir)? {
        let path = dir.join(&entry.name);
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        if entry.is_symlink {
            bail!("refusing to follow symbolic link '{}'", path.display());
        }
        checkpoints.push(read(&path)?);
    }
    checkpoints.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Ok(checkpoints)
}

/// Checks a `--resume`'s current top-level step labels (`app::
/// top_level_step_labels`) against `checkpoint.top_level_labels`: a mismatch
/// at or before `checkpoint.completed_index` is an error — resuming would
/// replay recorded state into a step list that has since changed underneath
/// it — while a mismatch only after that point (steps that haven't run yet)
/// is a warning, since resuming into a workflow whose *remaining* steps
/// changed is still meaningful.
pub(crate) fn check_resumable(current_labels: &[String], checkpoint: &Checkpoint) -> Result<()> {
    let recorded = &checkpoint.top_level_labels;
    let boundary = checkpoint.completed_index;
    if current_labels.len() < boundary || recorded.len() < boundary {
        bail!(
            "run '{}' was checkpointed after completing {boundary} of {} top-level step(s), but \
             the current workflow only has {}; resuming would replay recorded state into a step \
             list that no longer has that many steps. Start a fresh run instead.",
            checkpoint.run_id,
            recorded.len(),
            current_labels.len(),
        );
    }
    if current_labels[..boundary] != recorded[..boundary] {
        bail!(
            "run '{}' was checkpointed against a different sequence of top-level steps than the \
             current workflow (one of the already-completed steps changed); resuming would \
             replay recorded state into the wrong steps. Start a fresh run instead.",
            checkpoint.run_id,
        );
    }
    if current_labels[boundary..] != recorded[boundary..] {
        eprintln!(
            "warning: the workflow's steps after step {boundary} differ from when run '{}' was \
             checkpointed; resuming anyway",
            checkpoint.run_id
        );
    }
    Ok(())
}

/// Runs `lait runs list`/`lait runs show <RUN_ID>`.
pub(crate) fn run(command: RunsCommand) -> Result<()> {
    match command.action {
        RunsAction::List => {
            let checkpoints = list()?;
            if checkpoints.is_empty() {
                println!(
                    "no checkpointed runs saved yet; start one with `lait run <FILE> <PROMPT> \
                     --checkpoint`"
                );
                return Ok(());
            }
            for checkpoint in checkpoints {
                let status = checkpoint.status.as_str();
                println!(
                    "{}  ({status}, step {}/{}, {})",
                    checkpoint.run_id,
                    checkpoint.completed_index,
                    checkpoint.top_level_labels.len(),
                    checkpoint.workflow_path,
                );
            }
            Ok(())
        }
        RunsAction::Show(args) => {
            let checkpoint = load(&args.run_id)?;
            let status = checkpoint.status.as_str();
            println!("run_id: {}", checkpoint.run_id);
            println!("workflow: {}", checkpoint.workflow_path);
            println!("status: {status}");
            println!(
                "completed: {}/{} top-level step(s)",
                checkpoint.completed_index,
                checkpoint.top_level_labels.len(),
            );
            println!("initial prompt: {}", checkpoint.initial_prompt);
            println!("current input: {}", checkpoint.current_input);
            if !checkpoint.vars.is_empty() {
                let vars = serde_json::to_string(&checkpoint.vars)
                    .context("failed to serialize checkpoint vars")?;
                println!("vars: {vars}");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Checkpoint, RunStatus, check_resumable, generate_run_id, run_path};

    fn checkpoint_with(top_level_labels: Vec<&str>, completed_index: usize) -> Checkpoint {
        Checkpoint {
            run_id: "test-run".to_owned(),
            workflow_path: "workflow.yml".to_owned(),
            initial_prompt: "hi".to_owned(),
            vars: serde_json::Map::new(),
            top_level_labels: top_level_labels.into_iter().map(str::to_owned).collect(),
            completed_index,
            counter: completed_index,
            current_input: "hi".to_owned(),
            steps_outputs: serde_json::Map::new(),
            status: RunStatus::Failed,
        }
    }

    #[test]
    fn check_resumable_accepts_an_unchanged_step_sequence() {
        let checkpoint = checkpoint_with(vec!["a", "b", "c"], 2);
        assert!(
            check_resumable(
                &["a".to_owned(), "b".to_owned(), "c".to_owned()],
                &checkpoint
            )
            .is_ok()
        );
    }

    #[test]
    fn check_resumable_accepts_a_change_only_after_the_completed_step() {
        let checkpoint = checkpoint_with(vec!["a", "b", "c"], 2);
        // step 'c' (index 2, after completed_index) renamed to 'd': allowed,
        // just a warning to stderr.
        assert!(
            check_resumable(
                &["a".to_owned(), "b".to_owned(), "d".to_owned()],
                &checkpoint
            )
            .is_ok()
        );
    }

    #[test]
    fn check_resumable_rejects_a_change_at_or_before_the_completed_step() {
        let checkpoint = checkpoint_with(vec!["a", "b", "c"], 2);
        // step 'b' (index 1, already completed) renamed to 'x': rejected.
        let error = check_resumable(
            &["a".to_owned(), "x".to_owned(), "c".to_owned()],
            &checkpoint,
        )
        .unwrap_err();
        assert!(error.to_string().contains("different sequence"));
    }

    #[test]
    fn check_resumable_rejects_a_workflow_with_fewer_steps_than_were_completed() {
        let checkpoint = checkpoint_with(vec!["a", "b", "c"], 2);
        let error = check_resumable(&["a".to_owned()], &checkpoint).unwrap_err();
        assert!(error.to_string().contains("no longer has that many steps"));
    }

    #[test]
    fn generate_run_id_satisfies_the_run_path_charset() {
        let id = generate_run_id();
        assert!(run_path(&id).is_ok(), "generated id '{id}' was rejected");
    }
}
