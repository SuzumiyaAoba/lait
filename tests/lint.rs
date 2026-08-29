mod support;

use support::{
    AgentMarkdownFile, ConfigDirectory, WorkflowFile, next_temp_path, run_lait_lint, test_command,
};

#[test]
fn lint_reports_ok_for_a_valid_workflow_file() {
    let workflow = WorkflowFile::new("nodes:\n  a:\n    prompt: hi\nsteps:\n  - use: a\n");

    let output = run_lait_lint(&[&workflow.path]);

    assert!(output.status.success(), "lait lint failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OK"), "stdout: {stdout}");
}

#[test]
fn lint_reports_ok_for_a_valid_agent_file() {
    let agent = AgentMarkdownFile::new("---\nname: city-fact\n---\nCity: {{ input.city }}\n");

    let output = run_lait_lint(&[&agent.path]);

    assert!(output.status.success(), "lait lint failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OK"), "stdout: {stdout}");
}

#[test]
fn lint_fails_on_a_workflow_file_with_no_steps() {
    let workflow = WorkflowFile::new("nodes:\n  a:\n    prompt: hi\nsteps: []\n");

    let output = run_lait_lint(&[&workflow.path]);

    assert!(
        !output.status.success(),
        "expected lint to fail on an empty 'steps' list"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("error:"), "stdout: {stdout}");
}

#[test]
fn lint_fails_on_an_agent_file_without_frontmatter() {
    let agent = AgentMarkdownFile::new("no frontmatter here\n");

    let output = run_lait_lint(&[&agent.path]);

    assert!(
        !output.status.success(),
        "expected lint to fail on a missing frontmatter delimiter"
    );
}

#[test]
fn lint_warns_about_an_unused_node() {
    let workflow = WorkflowFile::new(
        "nodes:\n  used:\n    prompt: hi\n  unused:\n    prompt: hi\nsteps:\n  - use: used\n",
    );

    let output = run_lait_lint(&[&workflow.path]);

    assert!(output.status.success(), "lait lint failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("warning:"), "stdout: {stdout}");
    assert!(stdout.contains("unused"), "stdout: {stdout}");
}

#[test]
fn lint_flags_an_unknown_mcp_server_name() {
    // A present-but-empty 'mcp_servers:' map: unlike `ConfigDirectory::empty()`
    // (no lait.config.yml at all, which makes `lint` skip the check entirely
    // rather than report every name as unknown), this exercises the "found a
    // config, but this name isn't in it" path.
    let config = ConfigDirectory::new("mcp_servers: {}\n");
    let workflow =
        WorkflowFile::new("nodes:\n  a:\n    prompt: hi\n    mcp: [nope]\nsteps:\n  - use: a\n");

    let output = test_command()
        .current_dir(config.path())
        .arg("lint")
        .arg(&workflow.path)
        .output()
        .expect("failed to execute lait lint");

    assert!(
        !output.status.success(),
        "expected lint to fail on an unknown MCP server name"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("unknown MCP server 'nope'"),
        "stdout: {stdout}"
    );
}

#[test]
fn lint_accepts_a_known_mcp_server_name() {
    let config = ConfigDirectory::new("mcp_servers:\n  known:\n    command: \"true\"\n");
    let workflow =
        WorkflowFile::new("nodes:\n  a:\n    prompt: hi\n    mcp: [known]\nsteps:\n  - use: a\n");

    let output = test_command()
        .current_dir(config.path())
        .arg("lint")
        .arg(&workflow.path)
        .output()
        .expect("failed to execute lait lint");

    assert!(output.status.success(), "lait lint failed: {output:?}");
}

#[test]
fn lint_flags_an_unknown_skill_name_in_an_agent_file() {
    // Present-but-empty 'skills:', for the same reason as
    // `lint_flags_an_unknown_mcp_server_name`.
    let config = ConfigDirectory::new("skills: {}\n");
    let agent = AgentMarkdownFile::new("---\nskills: [nope]\n---\nbody\n");

    let output = test_command()
        .current_dir(config.path())
        .arg("lint")
        .arg(&agent.path)
        .output()
        .expect("failed to execute lait lint");

    assert!(
        !output.status.success(),
        "expected lint to fail on an unknown skill name"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("unknown skill 'nope'"), "stdout: {stdout}");
}

#[test]
fn lint_skips_mcp_and_skill_checks_and_still_succeeds_without_a_config_file() {
    let config = ConfigDirectory::empty();
    let workflow =
        WorkflowFile::new("nodes:\n  a:\n    prompt: hi\n    mcp: [nope]\nsteps:\n  - use: a\n");

    // `--no-config` is a global flag, but `Cli::args_conflicts_with_subcommands`
    // means it must come after the subcommand's own args, not before (see
    // `cli.rs`'s `run_subcommand_accepts_global_no_config_after_its_args`).
    let output = test_command()
        .current_dir(config.path())
        .arg("lint")
        .arg(&workflow.path)
        .arg("--no-config")
        .output()
        .expect("failed to execute lait lint");

    assert!(output.status.success(), "lait lint failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("unknown MCP"),
        "expected the mcp name check to be skipped: {stdout}"
    );
    assert!(stdout.contains("were not checked"), "stdout: {stdout}");
}

#[test]
fn lint_checks_every_file_even_after_an_earlier_one_fails() {
    let bad_workflow = WorkflowFile::new("steps: []\n");
    let good_agent = AgentMarkdownFile::new("---\n---\nhello\n");

    let output = run_lait_lint(&[&bad_workflow.path, &good_agent.path]);

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(bad_workflow.path.to_str().unwrap()),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(good_agent.path.to_str().unwrap()),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("OK"),
        "expected the good agent to still be checked: {stdout}"
    );
}

#[test]
fn lint_rejects_an_unrecognized_file_extension() {
    let path = next_temp_path("lait-test-lint", ".txt");
    std::fs::write(&path, "irrelevant").expect("failed to write fixture file");

    let output = run_lait_lint(&[&path]);

    std::fs::remove_file(&path).ok();

    assert!(
        !output.status.success(),
        "expected lint to reject an unrecognized file extension"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cannot determine the file type"),
        "stdout: {stdout}"
    );
}

#[test]
fn lint_detects_a_workflow_call_cycle() {
    let a_path = next_temp_path("lait-test-lint-cycle-a", ".yml");
    let b_path = next_temp_path("lait-test-lint-cycle-b", ".yml");

    std::fs::write(
        &a_path,
        format!(
            "nodes:\n  sub:\n    workflow: {}\nsteps:\n  - use: sub\n",
            b_path.file_name().unwrap().to_str().unwrap()
        ),
    )
    .expect("failed to write cycle test file a");
    std::fs::write(
        &b_path,
        format!(
            "nodes:\n  sub:\n    workflow: {}\nsteps:\n  - use: sub\n",
            a_path.file_name().unwrap().to_str().unwrap()
        ),
    )
    .expect("failed to write cycle test file b");

    let output = run_lait_lint(&[&a_path]);

    std::fs::remove_file(&a_path).ok();
    std::fs::remove_file(&b_path).ok();

    assert!(
        !output.status.success(),
        "expected lint to reject a workflow: cycle"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cycle"), "stdout: {stdout}");
    assert!(
        stdout.contains("in 'workflow:"),
        "expected the cycle message to be attributed to the sub-workflow that found it: {stdout}"
    );
}

#[test]
fn lint_flags_an_agent_referenced_by_a_node_that_does_not_exist() {
    let workflow = WorkflowFile::new(
        "nodes:\n  a:\n    agent: /nonexistent/lait-lint-test-agent.md\nsteps:\n  - use: a\n",
    );

    let output = run_lait_lint(&[&workflow.path]);

    assert!(
        !output.status.success(),
        "expected lint to fail on a node's missing agent file"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("failed to load"), "stdout: {stdout}");
}

#[test]
fn lint_flags_an_invalid_jq_filter_in_a_when_condition() {
    let workflow =
        WorkflowFile::new("nodes:\n  a:\n    prompt: hi\nsteps:\n  - use: a\n    when: \".[\"\n");

    let output = run_lait_lint(&[&workflow.path]);

    assert!(
        !output.status.success(),
        "expected lint to fail on an invalid jq filter"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("'when'"), "stdout: {stdout}");
}
