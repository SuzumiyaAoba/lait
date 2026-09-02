mod support;

use std::fs;

use support::{ConfigDirectory, test_command};

fn write(dir: &ConfigDirectory, relative: &str, contents: &str) {
    let path = dir.path().join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create test registry directory");
    }
    fs::write(&path, contents).expect("failed to write test registry file");
}

#[test]
fn workflow_list_shows_name_path_and_description() {
    let dir = ConfigDirectory::new("workflows:\n  hello: ./workflows/hello.yml\n");
    write(
        &dir,
        "workflows/hello.yml",
        "description: says hello\nnodes:\n  echo:\n    type: transform\n    jq: '.'\nsteps:\n  - use: echo\n",
    );

    // `--config` is a global flag, but `args_conflicts_with_subcommands`
    // means it must follow the subcommand token, not precede it.
    let output = test_command()
        .args([
            "workflow",
            "list",
            "--config",
            dir.config_path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute lait workflow list");

    assert!(output.status.success(), "workflow list failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello"), "stdout: {stdout}");
    assert!(stdout.contains("says hello"), "stdout: {stdout}");
}

#[test]
fn workflow_list_notes_a_missing_file_without_aborting() {
    let dir = ConfigDirectory::new(
        "workflows:\n  broken: ./workflows/missing.yml\n  hello: ./workflows/hello.yml\n",
    );
    write(
        &dir,
        "workflows/hello.yml",
        "nodes:\n  echo:\n    type: transform\n    jq: '.'\nsteps:\n  - use: echo\n",
    );

    let output = test_command()
        .args([
            "workflow",
            "list",
            "--config",
            dir.config_path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute lait workflow list");

    assert!(output.status.success(), "workflow list failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("broken"), "stdout: {stdout}");
    assert!(stdout.contains("hello"), "stdout: {stdout}");
    assert!(stdout.contains("warning:"), "stdout: {stdout}");
}

#[test]
fn agent_list_shows_name_path_and_description() {
    let dir = ConfigDirectory::new("agents:\n  greeter: ./agents/greeter.md\n");
    write(
        &dir,
        "agents/greeter.md",
        "---\ndescription: greets the user\n---\nHello {{ input }}\n",
    );

    let output = test_command()
        .args([
            "agent",
            "list",
            "--config",
            dir.config_path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute lait agent list");

    assert!(output.status.success(), "agent list failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("greeter"), "stdout: {stdout}");
    assert!(stdout.contains("greets the user"), "stdout: {stdout}");
}

#[test]
fn skill_list_shows_name_path_and_description() {
    let dir = ConfigDirectory::new("skills:\n  brief: ./skills/brief.md\n");
    write(
        &dir,
        "skills/brief.md",
        "---\ndescription: keep it brief\n---\nBe concise.\n",
    );

    let output = test_command()
        .args([
            "skill",
            "list",
            "--config",
            dir.config_path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute lait skill list");

    assert!(output.status.success(), "skill list failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("brief"), "stdout: {stdout}");
    assert!(stdout.contains("keep it brief"), "stdout: {stdout}");
}

#[test]
fn run_resolves_a_registered_workflow_name_relative_to_the_config_file() {
    let dir = ConfigDirectory::new("workflows:\n  hello: ./workflows/hello.yml\n");
    write(
        &dir,
        "workflows/hello.yml",
        "nodes:\n  echo:\n    type: transform\n    jq: '.'\nsteps:\n  - use: echo\n",
    );

    let output = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "hello",
            "world",
            "--config",
            dir.config_path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute lait run hello");

    assert!(output.status.success(), "lait run hello failed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolved 'hello'"),
        "expected a note about registry resolution, stderr: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("world"), "stdout: {stdout}");
}

#[test]
fn run_prefers_an_existing_file_over_a_same_named_registry_entry() {
    let dir = ConfigDirectory::new("workflows:\n  hello: ./workflows/hello.yml\n");
    write(
        &dir,
        "workflows/hello.yml",
        "nodes:\n  echo:\n    type: transform\n    jq: '\"from registry\"'\nsteps:\n  - use: echo\n",
    );
    write(
        &dir,
        "hello",
        "nodes:\n  echo:\n    type: transform\n    jq: '\"from file\"'\nsteps:\n  - use: echo\n",
    );

    let output = test_command()
        .current_dir(dir.path())
        .args([
            "run",
            "hello",
            "null",
            "--config",
            dir.config_path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute lait run hello");

    assert!(output.status.success(), "lait run hello failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "from file");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("also a 'workflows:' entry"),
        "expected a shadowing note, stderr: {stderr}"
    );
}

#[test]
fn lint_reports_a_missing_workflows_registry_path() {
    let dir = ConfigDirectory::new("workflows:\n  broken: ./workflows/missing.yml\n");
    write(
        &dir,
        "some.yml",
        "nodes:\n  a:\n    type: transform\n    jq: '.'\nsteps:\n  - use: a\n",
    );

    let output = test_command()
        .args([
            "lint",
            dir.path().join("some.yml").to_str().unwrap(),
            "--config",
            dir.config_path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute lait lint");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("broken"), "stdout: {stdout}");
    assert!(stdout.contains("no such file"), "stdout: {stdout}");
}

#[test]
fn lint_reports_ok_for_a_valid_workflows_registry() {
    let dir = ConfigDirectory::new("workflows:\n  hello: ./workflows/hello.yml\n");
    write(
        &dir,
        "workflows/hello.yml",
        "nodes:\n  echo:\n    type: transform\n    jq: '.'\nsteps:\n  - use: echo\n",
    );
    write(
        &dir,
        "some.yml",
        "nodes:\n  a:\n    type: transform\n    jq: '.'\nsteps:\n  - use: a\n",
    );

    let output = test_command()
        .args([
            "lint",
            dir.path().join("some.yml").to_str().unwrap(),
            "--config",
            dir.config_path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute lait lint");

    assert!(output.status.success(), "lait lint failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello: OK"), "stdout: {stdout}");
}
