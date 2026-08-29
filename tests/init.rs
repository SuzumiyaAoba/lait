mod support;

use support::{ConfigDirectory, test_command};

#[test]
fn init_creates_a_config_file_and_refuses_to_overwrite_it() {
    let dir = ConfigDirectory::empty();

    let output = test_command()
        .current_dir(dir.path())
        .arg("init")
        .output()
        .expect("failed to execute lait init");
    assert!(output.status.success(), "lait init failed: {output:?}");
    let config = std::fs::read_to_string(dir.config_path()).expect("config should be created");
    assert!(config.contains("default:"));

    let second = test_command()
        .current_dir(dir.path())
        .arg("init")
        .output()
        .expect("failed to execute lait init");
    assert!(!second.status.success(), "a second init must refuse");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already exists"),
        "the refusal should say why: {stderr}"
    );
}

#[test]
fn init_workflow_creates_a_scaffold_that_passes_lint() {
    let dir = ConfigDirectory::empty();
    let workflow_path = dir.path().join("workflow.yml");

    let output = test_command()
        .current_dir(dir.path())
        .args(["init", "workflow"])
        .output()
        .expect("failed to execute lait init workflow");
    assert!(output.status.success(), "lait init failed: {output:?}");
    assert!(workflow_path.is_file());

    let lint = test_command()
        .current_dir(dir.path())
        .args(["lint", "workflow.yml"])
        .output()
        .expect("failed to execute lait lint");
    assert!(
        lint.status.success(),
        "the workflow scaffold should pass lint: {lint:?}"
    );
}

#[test]
fn init_agent_creates_a_scaffold_that_passes_lint_at_a_custom_path() {
    let dir = ConfigDirectory::empty();
    let agent_path = dir.path().join("custom-agent.md");

    let output = test_command()
        .current_dir(dir.path())
        .args(["init", "agent", "custom-agent.md"])
        .output()
        .expect("failed to execute lait init agent");
    assert!(output.status.success(), "lait init failed: {output:?}");
    assert!(agent_path.is_file());

    let lint = test_command()
        .current_dir(dir.path())
        .args(["lint", "custom-agent.md"])
        .output()
        .expect("failed to execute lait lint");
    assert!(
        lint.status.success(),
        "the agent scaffold should pass lint: {lint:?}"
    );
}

#[test]
fn init_rejects_a_path_without_a_kind() {
    let output = test_command()
        .args(["init", "some-path.yml"])
        .output()
        .expect("failed to execute lait init");
    assert!(!output.status.success());
}
