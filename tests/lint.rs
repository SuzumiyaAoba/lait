mod support;

use support::{
    AgentMarkdownFile, ConfigDirectory, WorkflowFile, next_temp_path, run_lait_lint, test_command,
};

#[test]
fn lint_reports_ok_for_a_valid_workflow_file() {
    let workflow =
        WorkflowFile::new("nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n");

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
    let workflow = WorkflowFile::new("nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps: []\n");

    let output = run_lait_lint(&[&workflow.path]);

    assert!(
        !output.status.success(),
        "expected lint to fail on an empty 'steps' list"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("error:"), "stdout: {stdout}");
    assert_eq!(
        output.status.code(),
        Some(3),
        "a lint failure should exit with code 3: {output:?}"
    );
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
        "nodes:\n  used:\n    type: prompt\n    prompt: hi\n  unused:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: used\n",
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
    let workflow = WorkflowFile::new(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    mcp: [nope]\nsteps:\n  - use: a\n",
    );

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
    let workflow = WorkflowFile::new(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    mcp: [known]\nsteps:\n  - use: a\n",
    );

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
    let workflow = WorkflowFile::new(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    mcp: [nope]\nsteps:\n  - use: a\n",
    );

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
            "nodes:\n  sub:\n    type: workflow\n    workflow: {}\nsteps:\n  - use: sub\n",
            b_path.file_name().unwrap().to_str().unwrap()
        ),
    )
    .expect("failed to write cycle test file a");
    std::fs::write(
        &b_path,
        format!(
            "nodes:\n  sub:\n    type: workflow\n    workflow: {}\nsteps:\n  - use: sub\n",
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
        "nodes:\n  a:\n    type: agent\n    agent: /nonexistent/lait-lint-test-agent.md\nsteps:\n  - use: a\n",
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
    let workflow = WorkflowFile::new(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n    when: \".[\"\n",
    );

    let output = run_lait_lint(&[&workflow.path]);

    assert!(
        !output.status.success(),
        "expected lint to fail on an invalid jq filter"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("'when'"), "stdout: {stdout}");
}

#[test]
fn lint_flags_an_invalid_node_system_prompt_template() {
    let workflow = WorkflowFile::new(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    system_prompt: \"{{ input\"\nsteps:\n  - use: a\n",
    );

    let output = run_lait_lint(&[&workflow.path]);

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("'system_prompt' template"),
        "stdout: {stdout}"
    );
}

#[test]
fn lint_flags_an_invalid_workflow_default_system_prompt_template() {
    let workflow = WorkflowFile::new(
        "default:\n  system_prompt: \"{{ input\"\nnodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n",
    );

    let output = run_lait_lint(&[&workflow.path]);

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("'system_prompt' template"),
        "stdout: {stdout}"
    );
}

#[test]
fn lint_warns_about_an_unrecognized_type_in_an_input_schema() {
    let workflow = WorkflowFile::new(
        "json_schemas:\n  bad:\n    schema:\n      type: object\n      properties:\n        age:\n          type: sting\nnodes:\n  a:\n    type: prompt\n    prompt: hi\n    input_schema: bad\nsteps:\n  - use: a\n",
    );

    let output = run_lait_lint(&[&workflow.path]);

    assert!(
        output.status.success(),
        "an unrecognized schema type should warn, not fail: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("warning:"), "stdout: {stdout}");
    assert!(stdout.contains("type: sting"), "stdout: {stdout}");
}

#[test]
fn lint_flags_an_empty_command_program() {
    let workflow = WorkflowFile::new(
        "nodes:\n  a:\n    type: command\n    command: [\"  \"]\nsteps:\n  - use: a\n",
    );

    let output = run_lait_lint(&[&workflow.path]);

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("command[0]"), "stdout: {stdout}");
}

#[test]
fn lint_flags_a_model_definition_with_both_api_key_and_api_key_cmd() {
    let config = ConfigDirectory::new(
        "models:\n  cloud:\n    - provider:\n        base_url: https://api.example.com/v1\n        api_key: plain-key\n        api_key_cmd: \"printf x\"\n      model_id: cloud-model\n",
    );
    let workflow =
        WorkflowFile::new("nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n");

    let output = test_command()
        .current_dir(config.path())
        .arg("lint")
        .arg(&workflow.path)
        .output()
        .expect("failed to execute lait lint");

    assert!(!output.status.success(), "expected lint to fail");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cloud") && stdout.contains("api_key") && stdout.contains("api_key_cmd"),
        "stdout: {stdout}"
    );
}

/// A temporary directory tree for `lait lint <DIR>` recursion tests, cleaned
/// up on drop. Distinct from `ConfigDirectory` (which always writes
/// `lait.config.yml`) — this one is just an empty directory the test fills
/// in itself.
struct TempLintDir {
    path: std::path::PathBuf,
}

impl TempLintDir {
    fn new() -> Self {
        let path = next_temp_path("lait-test-lint-dir", "");
        std::fs::create_dir(&path).expect("failed to create temp lint directory");
        Self { path }
    }

    fn write(&self, relative: &str, contents: &str) {
        let full_path = self.path.join(relative);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create nested lint directory");
        }
        std::fs::write(&full_path, contents).expect("failed to write lint fixture file");
    }
}

impl Drop for TempLintDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn lint_recurses_into_a_directory_argument() {
    let dir = TempLintDir::new();
    dir.write(
        "sub/workflow.yml",
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n",
    );
    dir.write("sub/agent.md", "---\nname: city-fact\n---\nbody\n");
    dir.write("sub/README.md", "# not an agent file, no frontmatter\n");

    let output = run_lait_lint(&[&dir.path]);

    assert!(output.status.success(), "lait lint failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("workflow.yml"), "stdout: {stdout}");
    assert!(stdout.contains("agent.md"), "stdout: {stdout}");
    assert!(!stdout.contains("README.md"), "stdout: {stdout}");
}

#[test]
fn lint_directory_recursion_skips_target_and_node_modules() {
    let dir = TempLintDir::new();
    dir.write(
        "top.yml",
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n",
    );
    // Deliberately invalid, so an accidental descent into either directory
    // would flip the overall exit code and show up in stdout.
    dir.write("target/build.yml", "steps: []\n");
    dir.write("node_modules/pkg/ci.yml", "steps: []\n");

    let output = run_lait_lint(&[&dir.path]);

    assert!(
        output.status.success(),
        "expected 'target'/'node_modules' to be skipped: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("build.yml"), "stdout: {stdout}");
    assert!(!stdout.contains("ci.yml"), "stdout: {stdout}");
}

#[test]
fn lint_format_json_reports_a_structured_error_finding() {
    let workflow = WorkflowFile::new("steps: []\n");

    let output = test_command()
        .arg("lint")
        .arg(&workflow.path)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute lait lint --format json");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let findings: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("not valid JSON: {error}\n{stdout}"));
    let findings = findings.as_array().expect("expected a JSON array");
    assert!(!findings.is_empty(), "stdout: {stdout}");
    let finding = &findings[0];
    assert_eq!(
        finding["file"].as_str(),
        workflow.path.to_str(),
        "stdout: {stdout}"
    );
    assert_eq!(
        finding["severity"].as_str(),
        Some("error"),
        "stdout: {stdout}"
    );
    assert!(finding["message"].is_string(), "stdout: {stdout}");
}

#[test]
fn lint_format_json_guesses_a_line_number_for_an_unused_node() {
    let workflow = WorkflowFile::new(
        "nodes:\n  used:\n    type: prompt\n    prompt: hi\n  unused:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: used\n",
    );

    let output = test_command()
        .arg("lint")
        .arg(&workflow.path)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute lait lint --format json");

    assert!(output.status.success(), "lait lint failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let findings: serde_json::Value = serde_json::from_str(&stdout).expect("stdout should be JSON");
    let findings = findings.as_array().expect("expected a JSON array");
    let finding = findings
        .iter()
        .find(|finding| finding["message"].as_str().unwrap_or("").contains("unused"))
        .unwrap_or_else(|| panic!("no 'unused' finding: {stdout}"));
    // `unused:` is declared on line 5 of the fixture above (1-based).
    assert_eq!(finding["line"].as_u64(), Some(5), "stdout: {stdout}");
}

#[test]
fn lint_format_github_reports_error_annotations() {
    let workflow = WorkflowFile::new("steps: []\n");

    let output = test_command()
        .arg("lint")
        .arg(&workflow.path)
        .arg("--format")
        .arg("github")
        .output()
        .expect("failed to execute lait lint --format github");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_prefix = format!("::error file={}", workflow.path.display());
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with(&expected_prefix)),
        "stdout: {stdout}"
    );
}

#[test]
fn lint_format_json_reports_config_registry_errors_with_a_null_line() {
    let config =
        ConfigDirectory::new("workflows:\n  missing: /nonexistent/lait-lint-missing.yml\n");
    let workflow =
        WorkflowFile::new("nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n");

    let output = test_command()
        .current_dir(config.path())
        .arg("lint")
        .arg(&workflow.path)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute lait lint --format json");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let findings: serde_json::Value = serde_json::from_str(&stdout).expect("stdout should be JSON");
    let findings = findings.as_array().expect("expected a JSON array");
    let finding = findings
        .iter()
        .find(|finding| {
            finding["message"]
                .as_str()
                .unwrap_or("")
                .contains("missing")
        })
        .unwrap_or_else(|| panic!("no registry finding: {stdout}"));
    // Compared canonicalized: on macOS, `TMPDIR`'s `/var/folders/...` is a
    // symlink lait's own `std::env::current_dir()`-based path resolution
    // (unlike this test's own `next_temp_path`) resolves through, to
    // `/private/var/folders/...`.
    let expected = std::fs::canonicalize(config.config_path())
        .expect("failed to canonicalize the test config path");
    let actual = std::path::PathBuf::from(
        finding["file"]
            .as_str()
            .unwrap_or_else(|| panic!("finding has no 'file': {stdout}")),
    );
    assert_eq!(actual, expected, "stdout: {stdout}");
    assert!(finding["line"].is_null(), "stdout: {stdout}");
}
