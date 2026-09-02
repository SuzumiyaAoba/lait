mod support;

use std::fs;

use support::{ConfigDirectory, test_command};

/// A workflow whose first step (`mark`) records one line to `marker.txt`
/// every time it actually runs and passes its input straight through, and
/// whose second step (`fail`) always exits non-zero — so a `--checkpoint`
/// run always fails right after step 1, and `marker.txt` having exactly one
/// line after a later `--resume` proves step 1 was not re-executed.
const TWO_STEP_WORKFLOW: &str = r#"
nodes:
  mark:
    type: command
    command: ["sh", "-c", "echo ran >> marker.txt; cat"]
  fail:
    type: command
    command: ["sh", "-c", "exit 1"]
steps:
  - use: mark
  - use: fail
"#;

/// `TWO_STEP_WORKFLOW` with `fail` replaced by a command that succeeds and
/// passes its input straight through — simulates the user fixing the
/// workflow between a failed checkpointed run and `--resume`.
const TWO_STEP_WORKFLOW_FIXED: &str = r#"
nodes:
  mark:
    type: command
    command: ["sh", "-c", "echo ran >> marker.txt; cat"]
  fail:
    type: command
    command: ["cat"]
steps:
  - use: mark
  - use: fail
"#;

fn marker_lines(dir: &ConfigDirectory) -> usize {
    fs::read_to_string(dir.path().join("marker.txt"))
        .map(|contents| contents.lines().count())
        .unwrap_or(0)
}

fn run_ids(dir: &ConfigDirectory) -> Vec<String> {
    let mut ids: Vec<String> = fs::read_dir(dir.path().join(".lait/runs"))
        .expect("failed to read .lait/runs")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(str::to_owned)
        })
        .collect();
    ids.sort();
    ids
}

#[test]
fn checkpoint_records_the_first_step_and_resume_does_not_rerun_it() {
    let dir = ConfigDirectory::empty();
    fs::write(dir.path().join("workflow.yml"), TWO_STEP_WORKFLOW)
        .expect("failed to write test workflow");

    let output = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "hello",
            "--checkpoint",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success(), "expected the run to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("run checkpointed as"), "stderr: {stderr}");
    assert!(stderr.contains("--resume"), "stderr: {stderr}");
    assert_eq!(marker_lines(&dir), 1);

    let ids = run_ids(&dir);
    assert_eq!(
        ids.len(),
        1,
        "expected exactly one checkpointed run: {ids:?}"
    );
    let run_id = &ids[0];

    // Fix the workflow (as if the user edited it after seeing the failure)
    // and resume: step 1 ('mark') must not run again.
    fs::write(dir.path().join("workflow.yml"), TWO_STEP_WORKFLOW_FIXED)
        .expect("failed to rewrite test workflow");

    let resumed = test_command()
        .current_dir(dir.path())
        .args(["run", "workflow.yml", "--resume", run_id, "--no-config"])
        .output()
        .expect("failed to execute lait run --resume");

    assert!(resumed.status.success(), "resumed run failed: {resumed:?}");
    assert_eq!(String::from_utf8_lossy(&resumed.stdout).trim(), "hello");
    assert_eq!(marker_lines(&dir), 1, "step 1 must not re-run on resume");

    let show = test_command()
        .current_dir(dir.path())
        .args(["runs", "show", run_id])
        .output()
        .expect("failed to execute lait runs show");
    assert!(show.status.success(), "runs show failed: {show:?}");
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    assert!(show_stdout.contains("status: completed"), "{show_stdout}");
    assert!(show_stdout.contains("2/2"), "{show_stdout}");
}

#[test]
fn resume_fails_clearly_for_an_unknown_run_id() {
    let dir = ConfigDirectory::empty();
    fs::write(dir.path().join("workflow.yml"), TWO_STEP_WORKFLOW_FIXED)
        .expect("failed to write test workflow");

    let output = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "--resume",
            "no-such-run",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run --resume");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no such checkpointed run"), "{stderr}");
}

#[test]
fn resume_rejects_a_different_workflow_file_than_the_one_checkpointed() {
    let dir = ConfigDirectory::empty();
    fs::write(dir.path().join("workflow.yml"), TWO_STEP_WORKFLOW)
        .expect("failed to write test workflow");

    let failed = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "hello",
            "--checkpoint",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run");
    assert!(!failed.status.success());
    let run_id = &run_ids(&dir)[0];

    fs::write(dir.path().join("other.yml"), TWO_STEP_WORKFLOW_FIXED)
        .expect("failed to write a second workflow file");

    let output = test_command()
        .current_dir(dir.path())
        .args(["run", "other.yml", "--resume", run_id, "--no-config"])
        .output()
        .expect("failed to execute lait run --resume");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("was checkpointed against workflow"),
        "{stderr}"
    );
}

#[test]
fn resuming_an_already_completed_run_fails_clearly() {
    let dir = ConfigDirectory::empty();
    fs::write(dir.path().join("workflow.yml"), TWO_STEP_WORKFLOW_FIXED)
        .expect("failed to write test workflow");

    let output = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "hello",
            "--checkpoint",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run");
    assert!(output.status.success(), "run failed: {output:?}");
    let run_id = &run_ids(&dir)[0];

    let resumed = test_command()
        .current_dir(dir.path())
        .args(["run", "workflow.yml", "--resume", run_id, "--no-config"])
        .output()
        .expect("failed to execute lait run --resume");

    assert!(!resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert!(stderr.contains("already completed"), "{stderr}");
}

#[test]
fn runs_list_reports_no_runs_before_any_checkpoint_and_the_run_after_one() {
    let dir = ConfigDirectory::empty();
    fs::write(dir.path().join("workflow.yml"), TWO_STEP_WORKFLOW_FIXED)
        .expect("failed to write test workflow");

    let empty = test_command()
        .current_dir(dir.path())
        .args(["runs", "list"])
        .output()
        .expect("failed to execute lait runs list");
    assert!(empty.status.success());
    assert!(
        String::from_utf8_lossy(&empty.stdout).contains("no checkpointed runs"),
        "{:?}",
        empty
    );

    let output = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "hello",
            "--checkpoint",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run");
    assert!(output.status.success(), "run failed: {output:?}");

    let listed = test_command()
        .current_dir(dir.path())
        .args(["runs", "list"])
        .output()
        .expect("failed to execute lait runs list");
    assert!(listed.status.success());
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("completed"), "{stdout}");
    assert!(stdout.contains("workflow.yml"), "{stdout}");
}

#[test]
fn a_run_without_checkpoint_records_nothing() {
    let dir = ConfigDirectory::empty();
    fs::write(dir.path().join("workflow.yml"), TWO_STEP_WORKFLOW_FIXED)
        .expect("failed to write test workflow");

    let output = test_command()
        .current_dir(dir.path())
        .args(["run", "workflow.yml", "hello", "--no-config"])
        .output()
        .expect("failed to execute lait run");
    assert!(output.status.success(), "run failed: {output:?}");

    assert!(!dir.path().join(".lait/runs").exists());
}

#[test]
fn resume_reuses_the_recorded_vars_when_var_is_not_repeated() {
    let dir = ConfigDirectory::empty();
    fs::write(
        dir.path().join("workflow.yml"),
        r#"
nodes:
  mark:
    type: command
    command: ["sh", "-c", "echo ran >> marker.txt; cat"]
  fail:
    type: command
    command: ["sh", "-c", "exit 1"]
  greet:
    type: transform
    jq: '$vars.name'
steps:
  - use: mark
  - use: fail
  - use: greet
"#,
    )
    .expect("failed to write test workflow");

    let failed = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "hello",
            "--checkpoint",
            "--var",
            "name=world",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run");
    assert!(!failed.status.success());
    let run_id = &run_ids(&dir)[0];

    fs::write(
        dir.path().join("workflow.yml"),
        r#"
nodes:
  mark:
    type: command
    command: ["sh", "-c", "echo ran >> marker.txt; cat"]
  fail:
    type: command
    command: ["cat"]
  greet:
    type: transform
    jq: '$vars.name'
steps:
  - use: mark
  - use: fail
  - use: greet
"#,
    )
    .expect("failed to rewrite test workflow");

    let resumed = test_command()
        .current_dir(dir.path())
        .args(["run", "workflow.yml", "--resume", run_id, "--no-config"])
        .output()
        .expect("failed to execute lait run --resume");

    assert!(resumed.status.success(), "resumed run failed: {resumed:?}");
    assert_eq!(String::from_utf8_lossy(&resumed.stdout).trim(), "world");
}

/// Locks in the documented behavior (docs/usage/ja/workflow.md, "実行の再開"):
/// a `--var` passed alongside `--resume` overrides the recorded value for
/// that run going forward — not just for the one resumed invocation.
#[test]
fn resume_with_var_persists_the_override_into_the_checkpoint() {
    let dir = ConfigDirectory::empty();
    let broken = r#"
nodes:
  mark:
    type: command
    command: ["sh", "-c", "echo ran >> marker.txt; cat"]
  fail:
    type: command
    command: ["sh", "-c", "exit 1"]
  greet:
    type: transform
    jq: '$vars.name'
steps:
  - use: mark
  - use: fail
  - use: greet
"#;
    fs::write(dir.path().join("workflow.yml"), broken).expect("failed to write test workflow");

    let failed = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "null",
            "--checkpoint",
            "--var",
            "name=world",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run");
    assert!(!failed.status.success());
    let run_id = run_ids(&dir)[0].clone();

    // Resume with a different --var; this attempt still fails at the same
    // step (the workflow is untouched), but the checkpoint must now record
    // 'universe', not the original 'world'.
    let resumed_with_override = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "workflow.yml",
            "--resume",
            &run_id,
            "--var",
            "name=universe",
            "--no-config",
        ])
        .output()
        .expect("failed to execute lait run --resume");
    assert!(!resumed_with_override.status.success());

    let show = test_command()
        .current_dir(dir.path())
        .args(["runs", "show", &run_id])
        .output()
        .expect("failed to execute lait runs show");
    assert!(show.status.success(), "runs show failed: {show:?}");
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    assert!(show_stdout.contains("universe"), "{show_stdout}");
    assert!(!show_stdout.contains("world"), "{show_stdout}");

    // Fix the workflow and resume once more without --var: the persisted
    // override ('universe'), not the original value, must be used.
    fs::write(
        dir.path().join("workflow.yml"),
        r#"
nodes:
  mark:
    type: command
    command: ["sh", "-c", "echo ran >> marker.txt; cat"]
  fail:
    type: command
    command: ["cat"]
  greet:
    type: transform
    jq: '$vars.name'
steps:
  - use: mark
  - use: fail
  - use: greet
"#,
    )
    .expect("failed to rewrite test workflow");

    let resumed = test_command()
        .current_dir(dir.path())
        .args(["run", "workflow.yml", "--resume", &run_id, "--no-config"])
        .output()
        .expect("failed to execute lait run --resume");
    assert!(resumed.status.success(), "resumed run failed: {resumed:?}");
    assert_eq!(String::from_utf8_lossy(&resumed.stdout).trim(), "universe");
}
