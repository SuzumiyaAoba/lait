use super::*;
use std::collections::HashMap;

fn parse_workflow_fixture(yaml: &str) -> workflow::WorkflowFile {
    workflow::parse_workflow(yaml).expect("fixture workflow should validate")
}

fn empty_config() -> ConfigFile {
    ConfigFile::default()
}

fn lint_fixture(wf: &workflow::WorkflowFile, config: Option<&ConfigFile>) -> Vec<LintIssue> {
    let mut ctx = LintCtx::new(config);
    let mut issues = Vec::new();
    let mut visited = Vec::new();
    lint_workflow_contents(wf, Path::new("."), &mut ctx, &mut issues, &mut visited);
    issues
}

#[test]
fn warns_about_a_node_defined_but_never_used() {
    let wf = parse_workflow_fixture(
        "nodes:\n  used:\n    type: prompt\n    prompt: hi\n  unused:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: used\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues
            .iter()
            .any(|issue| issue.severity == Severity::Warning && issue.message.contains("unused")),
        "{issues:?}"
    );
    assert!(
        !issues.iter().any(|issue| issue.message.contains("'used'")),
        "{issues:?}"
    );
}

#[test]
fn does_not_warn_when_every_node_is_used() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn counts_a_node_used_only_inside_a_switch_case_as_used() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - switch:\n      cases:\n        - when: \".x\"\n          steps:\n            - use: a\n      else:\n        - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        !issues
            .iter()
            .any(|issue| issue.message.contains("never referenced")),
        "{issues:?}"
    );
}

#[test]
fn flags_an_invalid_jq_when_filter() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - use: a\n    when: \".[\"\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues
            .iter()
            .any(|issue| issue.severity == Severity::Error && issue.message.contains("'when'")),
        "{issues:?}"
    );
}

#[test]
fn flags_an_invalid_jq_for_each_items_filter() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\nsteps:\n  - for_each:\n      items: \".[\"\n      steps:\n        - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues.iter().any(|issue| issue.message.contains("'items'")),
        "{issues:?}"
    );
}

#[test]
fn flags_an_invalid_prompt_template() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: \"{{ input\"\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues.iter().any(|issue| issue.severity == Severity::Error
            && issue.message.contains("'prompt' template")),
        "{issues:?}"
    );
}

#[test]
fn flags_an_invalid_command_argument_template() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: command\n    command: [\"echo\", \"{{ input\"]\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues.iter().any(|issue| issue.severity == Severity::Error
            && issue.message.contains("'command' argument template")),
        "{issues:?}"
    );
}

#[test]
fn accepts_a_bare_input_placeholder_in_a_prompt() {
    // A scalar `{{ input }}` (the common case for a first step run
    // against a plain-text CLI argument) is valid; only `render`, at
    // actual render time against real data, can know whether the input
    // will be an object/array — see `template::check_syntax`'s doc
    // comment.
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: \"{{ input }}\"\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn flags_an_unknown_mcp_server_name() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    mcp: [nope]\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("unknown MCP server 'nope'")),
        "{issues:?}"
    );
}

#[test]
fn accepts_a_known_mcp_server_name() {
    let mut config = empty_config();
    config.mcp_servers.insert(
        "known".to_owned(),
        config::McpServerConfig {
            command: Some("true".to_owned()),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: HashMap::new(),
            allowed_tools: None,
        },
    );
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    mcp: [known]\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&config));
    assert!(
        !issues.iter().any(|issue| issue.message.contains("MCP")),
        "{issues:?}"
    );
}

#[test]
fn flags_a_referenced_mcp_server_whose_allowed_tools_is_empty() {
    let mut config = empty_config();
    config.mcp_servers.insert(
        "locked-down".to_owned(),
        config::McpServerConfig {
            command: Some("true".to_owned()),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: HashMap::new(),
            allowed_tools: Some(Vec::new()),
        },
    );
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    mcp: [locked-down]\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&config));
    assert!(
        issues.iter().any(|issue| {
            issue.severity == Severity::Warning
                && issue.message.contains("locked-down")
                && issue.message.contains("allowed_tools")
        }),
        "{issues:?}"
    );
}

#[test]
fn flags_an_unknown_skill_name() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    skills: [nope]\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("unknown skill 'nope'")),
        "{issues:?}"
    );
}

#[test]
fn flags_an_unknown_subagent_name() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    subagents: [nope]\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("unknown subagent 'nope'")),
        "{issues:?}"
    );
}

#[test]
fn accepts_a_known_subagent_name() {
    let mut config = empty_config();
    config
        .agents
        .insert("known".to_owned(), PathBuf::from("agents/known.md"));
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    subagents: [known]\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&config));
    assert!(
        !issues
            .iter()
            .any(|issue| issue.message.contains("subagent")),
        "{issues:?}"
    );
}

#[test]
fn skips_mcp_and_skill_checks_and_notes_it_when_there_is_no_config() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    mcp: [nope]\nsteps:\n  - use: a\n",
    );
    let mut ctx = LintCtx::new(None);
    let mut issues = Vec::new();
    let mut visited = Vec::new();
    lint_workflow_contents(&wf, Path::new("."), &mut ctx, &mut issues, &mut visited);
    note_skipped_capability_check(&mut ctx, &mut issues);
    assert!(
        !issues
            .iter()
            .any(|issue| issue.message.contains("unknown MCP"))
    );
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("were not checked")),
        "{issues:?}"
    );
}

#[test]
fn flags_an_unresolvable_output_schema_name() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: prompt\n    prompt: hi\n    output_schema: nonexistent.json\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("unresolvable 'output_schema'")),
        "{issues:?}"
    );
}

#[test]
fn accepts_an_output_schema_name_defined_in_json_schemas() {
    let wf = parse_workflow_fixture(
        "json_schemas:\n  city:\n    schema:\n      type: object\nnodes:\n  a:\n    type: prompt\n    prompt: hi\n    output_schema: city\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        !issues
            .iter()
            .any(|issue| issue.message.contains("output_schema")),
        "{issues:?}"
    );
}

#[test]
fn flags_a_schema_name_with_an_invalid_character() {
    // `output_schema` alone (this fixture's `schema_name` is unset,
    // defaulting to "structured_output", which is valid) isn't enough to
    // trigger this — the invalid character has to actually be spelled
    // out in `schema_name`.
    let wf = parse_workflow_fixture(
        "json_schemas:\n  city:\n    schema:\n      type: object\nnodes:\n  a:\n    type: prompt\n    prompt: hi\n    output_schema: city\n    schema_name: \"bad name!\"\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("invalid 'schema_name'")),
        "{issues:?}"
    );
}

#[test]
fn accepts_the_default_schema_name_when_none_is_set() {
    let wf = parse_workflow_fixture(
        "json_schemas:\n  city:\n    schema:\n      type: object\nnodes:\n  a:\n    type: prompt\n    prompt: hi\n    output_schema: city\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        !issues
            .iter()
            .any(|issue| issue.message.contains("schema_name")),
        "{issues:?}"
    );
}

#[test]
fn flags_a_missing_agent_file() {
    let wf = parse_workflow_fixture(
        "nodes:\n  a:\n    type: agent\n    agent: /nonexistent/agent-does-not-exist.md\nsteps:\n  - use: a\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("failed to load")),
        "{issues:?}"
    );
}

/// A `.md` file at a unique path under the system temp directory,
/// removed on drop. `lint.rs`'s own tests use this directly (rather than
/// `tests/support::AgentMarkdownFile`, an integration-test-only helper
/// this binary crate's unit tests can't reach) for the handful of checks
/// that need a real agent file on disk (`agent::load_agent` reads from a
/// path, not a string).
struct TempAgentFile {
    path: PathBuf,
}

impl TempAgentFile {
    fn new(contents: &str) -> Self {
        // A counter alongside the nanosecond timestamp: `cargo test` runs
        // these concurrently on multiple threads, and two calls can land
        // on the same nanosecond on a coarse-resolution clock, which
        // would otherwise make the second `fs::write` silently overwrite
        // the first test's file out from under it.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "lait-lint-test-agent-{}-{unique}-{counter}.md",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("failed to write fixture agent file");
        Self { path }
    }
}

impl Drop for TempAgentFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn agent_lint_flags_an_unknown_skill_name() {
    let agent = TempAgentFile::new("---\nskills: [nope]\n---\nbody\n");
    let report = lint_agent_file(&agent.path, Some(&empty_config()));
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.message.contains("unknown skill 'nope'")),
        "{:?}",
        report.issues
    );
}

#[test]
fn agent_lint_flags_an_unknown_subagent_name() {
    let agent = TempAgentFile::new("---\nsubagents: [nope]\n---\nbody\n");
    let report = lint_agent_file(&agent.path, Some(&empty_config()));
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.message.contains("unknown subagent 'nope'")),
        "{:?}",
        report.issues
    );
}

#[test]
fn agent_lint_flags_an_invalid_system_prompt_template() {
    let agent = TempAgentFile::new("---\n---\n{{ input\n");
    let report = lint_agent_file(&agent.path, Some(&empty_config()));
    assert!(
        report.has_errors(),
        "expected an invalid template to be flagged: {:?}",
        report.issues
    );
}

#[test]
fn agent_lint_flags_an_invalid_schema_name() {
    let agent = TempAgentFile::new(
        "---\noutput_schema:\n  schema:\n    type: object\nstructured_output: true\nschema_name: \"bad name!\"\n---\nbody\n",
    );
    let report = lint_agent_file(&agent.path, Some(&empty_config()));
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.message.contains("invalid 'schema_name'")),
        "{:?}",
        report.issues
    );
}

#[test]
fn agent_lint_reports_a_parse_error_as_a_single_issue() {
    let agent = TempAgentFile::new("no frontmatter here\n");
    let report = lint_agent_file(&agent.path, Some(&empty_config()));
    assert_eq!(report.issues.len(), 1, "{:?}", report.issues);
    assert!(report.has_errors());
}

#[test]
fn lint_file_rejects_an_unrecognized_extension() {
    assert!(lint_file(Path::new("thing.txt"), Some(&empty_config())).is_err());
}

#[test]
fn yaml_error_line_reports_the_parser_location() {
    let error = serde_yaml::from_str::<serde_yaml::Value>("steps: [\n")
        .expect_err("malformed YAML should fail to parse");
    let line = yaml_error_line(&anyhow::Error::new(error));
    assert!(line.is_some(), "expected a line number from the parser");
}
