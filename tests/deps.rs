//! Integration tests for `lait deps` — the GitHub dependency commands.
//! Every test points `GITHUB_API_URL` at a local `MockGitHub` (see
//! `tests/support`), so no real network access happens; the mock's route
//! table stands in for ref resolution, contents downloads, and (via the
//! recorded requests) bearer authentication.

mod support;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use support::{MockGitHub, MockServer, ScratchDir, completion_body, test_command};

/// A `lait` invocation rooted at `project` and pointed at the mock GitHub
/// API. `GITHUB_TOKEN`/`GH_TOKEN` are scrubbed unless a test sets one —
/// a developer's real token must never leak into these runs (and the auth
/// assertion needs a known value anyway).
fn deps_command(project: &Path, github: &MockGitHub) -> Command {
    let mut command = test_command();
    command
        .current_dir(project)
        .env("GITHUB_API_URL", github.api_url())
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN");
    command
}

fn run_deps(project: &Path, github: &MockGitHub, args: &[&str]) -> Output {
    deps_command(project, github)
        .arg("deps")
        .args(args)
        .output()
        .expect("failed to execute lait deps")
}

/// The payload file `deps add`/`install` materializes for `name`.
fn payload(project: &Path, name: &str, file: &str) -> std::path::PathBuf {
    project.join(".lait").join("deps").join(name).join(file)
}

const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A dep that needs no model: a `transform`/`jq` workflow echoes its input.
const DEP_WORKFLOW_YML: &str =
    "nodes:\n  echo:\n    type: transform\n    jq: '\"from dep\"'\nsteps:\n  - use: echo\n";

const DEP_AGENT_MD: &str =
    "---\nname: dep-agent\ndescription: fetched agent\n---\nEcho {{ input }}\n";

const DEP_SKILL_MD: &str =
    "---\nname: dep-style\ndescription: fetched skill\n---\nAlways answer tersely.\n";

/// Registers `slug`'s repo on `main` at `commit` with `path` → `body`.
fn publish(github: &MockGitHub, slug: &str, commit: &str, path: &str, body: &str) {
    github.add_repo(slug, "main");
    github.set_commit(slug, "main", commit);
    github.set_file(slug, commit, path, body);
}

#[test]
fn deps_add_fetches_pins_and_materializes_a_workflow() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();

    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml@main"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");

    // The manifest records the canonical source spelling and the ref.
    let manifest = fs::read_to_string(project.path().join("lait.deps.yml")).unwrap();
    assert!(manifest.contains("review:"), "manifest: {manifest}");
    assert!(
        manifest.contains("source: github:o/r/workflows/review.yml"),
        "manifest: {manifest}"
    );
    assert!(manifest.contains("ref: main"), "manifest: {manifest}");
    assert!(manifest.contains("kind: workflow"), "manifest: {manifest}");

    // The lock pins the resolved commit and the payload digest.
    let lock = fs::read_to_string(project.path().join("lait.lock")).unwrap();
    assert!(lock.contains(COMMIT_A), "lock: {lock}");
    assert!(lock.contains("sha256:"), "lock: {lock}");

    // The payload landed under .lait/deps/<name>/<basename>.
    let materialized = payload(project.path(), "review", "review.yml");
    assert_eq!(fs::read_to_string(&materialized).unwrap(), DEP_WORKFLOW_YML);
}

#[test]
fn run_resolves_a_dependency_workflow_by_name() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();

    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");

    // No lait.config.yml at all: the dep merges into `workflows:` through
    // the manifest alone, so `lait run review` finds it by name.
    let output = test_command()
        .current_dir(project.path())
        .args(["run", "review", "hi"])
        .output()
        .expect("failed to execute lait run");
    assert!(
        output.status.success(),
        "lait run review failed: {output:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "from dep");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolved 'review'") && stderr.contains("'workflows:'"),
        "expected a registry-resolution note, stderr: {stderr}"
    );
}

#[test]
fn agent_run_resolves_a_dependency_agent_by_name() {
    let github = MockGitHub::start();
    publish(&github, "o/r", COMMIT_A, "agents/helper.md", DEP_AGENT_MD);
    let llm = MockServer::start("200 OK", &completion_body("test-model", "agent reply"));
    let project = ScratchDir::new();
    project.write(
        "lait.config.yml",
        &format!(
            "base_url: \"{}\"\ndefault:\n  model: test-model\n",
            llm.base_url
        ),
    );

    let output = run_deps(project.path(), &github, &["add", "o/r/agents/helper.md"]);
    assert!(output.status.success(), "deps add failed: {output:?}");

    let output = test_command()
        .current_dir(project.path())
        .args(["agent", "run", "helper", "hi there"])
        .output()
        .expect("failed to execute lait agent run");
    let request = llm.receive_request();
    llm.finish();

    assert!(output.status.success(), "lait agent run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "agent reply"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolved 'helper'") && stderr.contains("'agents:'"),
        "expected an 'agents:' resolution note, stderr: {stderr}"
    );
    let request_json: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(request_json["messages"][0]["content"], "Echo hi there");
}

#[test]
fn deps_add_skill_and_agent_uses_it_by_name() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "skills/dep-style/SKILL.md",
        DEP_SKILL_MD,
    );
    let llm = MockServer::start("200 OK", &completion_body("test-model", "ok"));
    let project = ScratchDir::new();
    project.write(
        "lait.config.yml",
        &format!(
            "base_url: \"{}\"\ndefault:\n  model: test-model\nagents:\n  a: ./a.md\n",
            llm.base_url
        ),
    );
    project.write(
        "a.md",
        "---\nskills: [dep-style]\n---\nAnswer {{ input }}\n",
    );

    // `skills/dep-style/SKILL.md` derives the name `dep-style` (the parent
    // directory, not the SKILL.md basename) and infers kind: skill.
    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/skills/dep-style/SKILL.md"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");
    assert!(payload(project.path(), "dep-style", "SKILL.md").is_file());

    let output = test_command()
        .current_dir(project.path())
        .args(["agent", "run", "a", "hi"])
        .output()
        .expect("failed to execute lait agent run");
    let request = llm.receive_request();
    llm.finish();

    assert!(output.status.success(), "lait agent run failed: {output:?}");
    let request_json: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    let system = request_json["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("Always answer tersely."),
        "skill body missing from the system prompt: {system}"
    );
}

#[test]
fn deps_install_reproduces_the_locked_commit_after_the_branch_moves() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();
    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");

    // The branch moves upstream and the local payload is wiped — install
    // must restore the *locked* commit, not the branch tip.
    fs::remove_dir_all(project.path().join(".lait/deps")).unwrap();
    github.set_commit("o/r", "main", COMMIT_B);
    github.set_file("o/r", COMMIT_B, "workflows/review.yml", "steps: []\n");

    let output = run_deps(project.path(), &github, &["install"]);
    assert!(output.status.success(), "deps install failed: {output:?}");
    assert_eq!(
        fs::read_to_string(payload(project.path(), "review", "review.yml")).unwrap(),
        DEP_WORKFLOW_YML,
        "install must materialize the locked commit's bytes"
    );
    let lock = fs::read_to_string(project.path().join("lait.lock")).unwrap();
    assert!(lock.contains(COMMIT_A), "lock: {lock}");
    assert!(!lock.contains(COMMIT_B), "lock: {lock}");
}

#[test]
fn deps_install_frozen_fails_on_a_manifest_the_lock_does_not_cover() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();
    // `@main` so the manifest records `ref: main` for the drift below.
    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml@main"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");

    // Drift the manifest: the request now names a different ref than the
    // lock covers.
    let manifest_path = project.path().join("lait.deps.yml");
    let manifest = fs::read_to_string(&manifest_path).unwrap();
    fs::write(&manifest_path, manifest.replace("ref: main", "ref: v2")).unwrap();

    let output = run_deps(project.path(), &github, &["install", "--frozen"]);
    assert!(
        !output.status.success(),
        "--frozen should reject a lock that doesn't cover the manifest"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not covered"), "stderr: {stderr}");
}

#[test]
fn deps_update_moves_the_lock_to_the_new_commit() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();
    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");

    let updated = "nodes:\n  echo:\n    type: transform\n    jq: '\"v2\"'\nsteps:\n  - use: echo\n";
    github.set_commit("o/r", "main", COMMIT_B);
    github.set_file("o/r", COMMIT_B, "workflows/review.yml", updated);

    let output = run_deps(project.path(), &github, &["update"]);
    assert!(output.status.success(), "deps update failed: {output:?}");
    assert_eq!(
        fs::read_to_string(payload(project.path(), "review", "review.yml")).unwrap(),
        updated
    );
    let lock = fs::read_to_string(project.path().join("lait.lock")).unwrap();
    assert!(lock.contains(COMMIT_B), "lock: {lock}");
}

#[test]
fn deps_remove_drops_the_manifest_lock_and_payload() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();
    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");

    let output = run_deps(project.path(), &github, &["remove", "review"]);
    assert!(output.status.success(), "deps remove failed: {output:?}");

    let manifest = fs::read_to_string(project.path().join("lait.deps.yml")).unwrap();
    assert!(!manifest.contains("review:"), "manifest: {manifest}");
    let lock = fs::read_to_string(project.path().join("lait.lock")).unwrap();
    assert!(!lock.contains("review"), "lock: {lock}");
    assert!(!project.path().join(".lait/deps/review").exists());
}

#[test]
fn deps_verify_flags_a_modified_payload_and_install_restores_it() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();
    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");

    fs::write(
        payload(project.path(), "review", "review.yml"),
        "steps: []\n",
    )
    .unwrap();
    let output = run_deps(project.path(), &github, &["verify"]);
    assert!(
        !output.status.success(),
        "verify should fail on a modified payload"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("review: FAILED"), "stdout: {stdout}");

    let output = run_deps(project.path(), &github, &["install"]);
    assert!(output.status.success(), "deps install failed: {output:?}");
    let output = run_deps(project.path(), &github, &["verify"]);
    assert!(
        output.status.success(),
        "verify after install failed: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("review: ok"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn deps_list_reports_each_dependency_and_its_status() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    publish(&github, "o/r", COMMIT_A, "agents/helper.md", DEP_AGENT_MD);
    let project = ScratchDir::new();
    for spec in ["o/r/workflows/review.yml", "o/r/agents/helper.md"] {
        let output = run_deps(project.path(), &github, &["add", spec]);
        assert!(
            output.status.success(),
            "deps add {spec} failed: {output:?}"
        );
    }
    fs::remove_file(payload(project.path(), "helper", "helper.md")).unwrap();

    let output = run_deps(project.path(), &github, &["list"]);
    assert!(output.status.success(), "deps list failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("review"), "stdout: {stdout}");
    assert!(stdout.contains("installed"), "stdout: {stdout}");
    assert!(stdout.contains("helper"), "stdout: {stdout}");
    assert!(stdout.contains("not installed"), "stdout: {stdout}");
}

#[test]
fn deps_add_sends_the_github_token_as_a_bearer() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();

    let output = deps_command(project.path(), &github)
        .env("GITHUB_TOKEN", "test-token-123")
        .args(["deps", "add", "o/r/workflows/review.yml@main"])
        .output()
        .expect("failed to execute lait deps add");
    assert!(output.status.success(), "deps add failed: {output:?}");

    let request = github.receive_request();
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-token-123"),
        "expected a bearer Authorization header, got: {}",
        request.headers
    );
}

#[test]
fn deps_add_rejects_content_that_does_not_parse_as_its_kind() {
    let github = MockGitHub::start();
    // `.yml` infers `kind: workflow`, but the body isn't a workflow file.
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/bad.yml",
        "not a: [workflow\n",
    );
    let project = ScratchDir::new();

    let output = run_deps(project.path(), &github, &["add", "o/r/workflows/bad.yml"]);
    assert!(
        !output.status.success(),
        "deps add should reject an invalid workflow file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a valid workflow file"),
        "stderr: {stderr}"
    );
    // A rejected add writes nothing: no manifest, no lock, no payload.
    assert!(!project.path().join("lait.deps.yml").exists());
    assert!(!project.path().join("lait.lock").exists());
    assert!(!project.path().join(".lait/deps").exists());
}

#[test]
fn deps_add_reports_a_github_error_for_an_unknown_repo() {
    let github = MockGitHub::start();
    let project = ScratchDir::new();

    let output = run_deps(project.path(), &github, &["add", "ghost/nope/f.yml@main"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("404"), "stderr: {stderr}");
    // With no token configured, the error should hint at GITHUB_TOKEN.
    assert!(stderr.contains("GITHUB_TOKEN"), "stderr: {stderr}");
}

#[test]
fn deps_add_rejects_a_malformed_spec() {
    let github = MockGitHub::start();
    let project = ScratchDir::new();

    let output = run_deps(project.path(), &github, &["add", "owner/repo"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("path"), "stderr: {stderr}");
}

#[test]
fn deps_add_name_and_kind_overrides() {
    let github = MockGitHub::start();
    // `.txt` can't infer a kind; --kind supplies it, --name the registry key.
    publish(&github, "o/r", COMMIT_A, "prompts/style.txt", DEP_SKILL_MD);
    let project = ScratchDir::new();

    let output = run_deps(
        project.path(),
        &github,
        &[
            "add",
            "o/r/prompts/style.txt",
            "--name",
            "style",
            "--kind",
            "skill",
        ],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");
    assert!(payload(project.path(), "style", "style.txt").is_file());
    let manifest = fs::read_to_string(project.path().join("lait.deps.yml")).unwrap();
    assert!(manifest.contains("style:"), "manifest: {manifest}");
    assert!(manifest.contains("kind: skill"), "manifest: {manifest}");
}

#[test]
fn deps_add_rejects_a_duplicate_name() {
    let github = MockGitHub::start();
    publish(
        &github,
        "o/r",
        COMMIT_A,
        "workflows/review.yml",
        DEP_WORKFLOW_YML,
    );
    let project = ScratchDir::new();
    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml"],
    );
    assert!(output.status.success(), "deps add failed: {output:?}");

    let output = run_deps(
        project.path(),
        &github,
        &["add", "o/r/workflows/review.yml"],
    );
    assert!(!output.status.success(), "duplicate add should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already exists"), "stderr: {stderr}");
}

#[test]
fn deps_install_with_no_manifest_is_a_noop() {
    let github = MockGitHub::start();
    let project = ScratchDir::new();
    let output = run_deps(project.path(), &github, &["install"]);
    assert!(output.status.success(), "deps install failed: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("nothing to install"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
