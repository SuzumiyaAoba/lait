use super::*;

fn parse_workflow_fixture(yaml: &str) -> workflow::WorkflowFile {
    workflow::parse_workflow(yaml).expect("fixture workflow should validate")
}

fn empty_config() -> ConfigFile {
    ConfigFile::default()
}

fn lint_fixture(wf: &workflow::WorkflowFile, config: Option<&ConfigFile>) -> Vec<LintIssue> {
    let mut ctx = LintCtx::new(config);
    lint_workflow_contents(wf, &mut ctx);
    ctx.issues
}

fn has(issues: &[LintIssue], severity: Severity, text: &str) -> bool {
    issues
        .iter()
        .any(|issue| issue.severity == severity && issue.message.contains(text))
}

#[test]
fn a_clean_workflow_has_no_issues() {
    let wf = parse_workflow_fixture(
        "inputs:\n  lang: string\nsteps:\n  - id: a\n    prompt: 'in {{ inputs.lang }}: {{ input }}'\n  - jq: '{a: $steps.a, l: $inputs.lang}'\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn flags_a_reference_to_an_undeclared_input() {
    let wf = parse_workflow_fixture(
        "inputs:\n  lang: string\nsteps:\n  - prompt: '{{ inputs.langg }}'\n  - jq: '$inputs.missing'\n    when: '$inputs.lang == \"ja\"'\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(has(&issues, Severity::Error, "input 'langg'"), "{issues:?}");
    assert!(
        has(&issues, Severity::Error, "input 'missing'"),
        "{issues:?}"
    );
    assert!(
        !has(&issues, Severity::Error, "input 'lang',"),
        "{issues:?}"
    );
}

#[test]
fn warns_about_a_reference_to_an_unknown_step_id() {
    let wf = parse_workflow_fixture(
        "steps:\n  - id: first\n    jq: .\n  - prompt: '{{ steps.frist }}'\n  - jq: '$steps[\"nope\"]'\n    output: '$steps.first'\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        has(&issues, Severity::Warning, "step 'frist'"),
        "{issues:?}"
    );
    assert!(has(&issues, Severity::Warning, "step 'nope'"), "{issues:?}");
    assert!(
        !has(&issues, Severity::Warning, "step 'first'"),
        "{issues:?}"
    );
}

#[test]
fn step_ids_inside_nested_steps_count_as_known() {
    let wf = parse_workflow_fixture(
        "steps:\n  - parallel:\n      a:\n        - id: inner\n          jq: .\n  - jq: '$steps.inner'\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(!has(&issues, Severity::Warning, "inner"), "{issues:?}");
}

#[test]
fn warns_about_unused_schemas_and_agents() {
    let wf = parse_workflow_fixture(
        "schemas:\n  used: {type: object}\n  unused: {type: object}\nagents:\n  idle:\n    system: hi\nsteps:\n  - prompt: x\n    output_schema: used\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        has(&issues, Severity::Warning, "schema 'unused'"),
        "{issues:?}"
    );
    assert!(
        !has(&issues, Severity::Warning, "schema 'used'"),
        "{issues:?}"
    );
    assert!(
        has(&issues, Severity::Warning, "agent 'idle'"),
        "{issues:?}"
    );
}

#[test]
fn flags_an_unloadable_schema_file() {
    let wf = parse_workflow_fixture(
        "schemas:\n  s: {file: /nonexistent/lait-schema.json}\nsteps:\n  - prompt: x\n    output_schema: s\n  - prompt: y\n    input_schema: {file: /nonexistent/other.json}\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        has(&issues, Severity::Error, "schema 's' is invalid"),
        "{issues:?}"
    );
    assert!(
        has(&issues, Severity::Error, "'input_schema' is invalid"),
        "{issues:?}"
    );
}

#[test]
fn warns_about_an_unrecognized_schema_type() {
    let wf = parse_workflow_fixture(
        "inputs:\n  n: { type: interger, default: 1 }\nsteps:\n  - prompt: x\n    input_schema: {type: object, properties: {a: {type: sting}}}\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        has(&issues, Severity::Warning, "'type: interger'"),
        "{issues:?}"
    );
    assert!(
        has(&issues, Severity::Warning, "'type: sting'"),
        "{issues:?}"
    );
}

#[test]
fn flags_unknown_capability_names_on_steps_defaults_and_inline_agents() {
    let wf = parse_workflow_fixture(
        "default:\n  skills: [nope-skill]\nagents:\n  a:\n    system: hi\n    tools: [nope-tool]\nsteps:\n  - prompt: x\n    mcp: [nope-mcp]\n  - agent: a\n    subagents: [nope-agent]\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    for text in [
        "unknown skill 'nope-skill'",
        "unknown tool 'nope-tool'",
        "unknown MCP server 'nope-mcp'",
        "unknown subagent 'nope-agent'",
    ] {
        assert!(has(&issues, Severity::Error, text), "{text}: {issues:?}");
    }
}

#[test]
fn accepts_known_capability_names() {
    let mut config = empty_config();
    std::sync::Arc::make_mut(&mut config.agents)
        .insert("researcher".to_owned(), PathBuf::from("researcher.md"));
    let wf = parse_workflow_fixture("steps:\n  - prompt: x\n    subagents: [researcher]\n");
    let issues = lint_fixture(&wf, Some(&config));
    assert!(!has(&issues, Severity::Error, "subagent"), "{issues:?}");
}

#[test]
fn flags_a_referenced_mcp_server_whose_allowed_tools_is_empty() {
    let config: ConfigFile = serde_yaml::from_str(
        "mcp_servers:\n  locked:\n    command: server\n    allowed_tools: []\n",
    )
    .unwrap();
    let wf = parse_workflow_fixture("steps:\n  - prompt: x\n    mcp: [locked]\n");
    let issues = lint_fixture(&wf, Some(&config));
    assert!(has(&issues, Severity::Warning, "empty list"), "{issues:?}");
}

#[test]
fn skips_capability_checks_and_notes_it_when_there_is_no_config() {
    let wf =
        parse_workflow_fixture("steps:\n  - prompt: x\n    mcp: [nope]\n  - agent: registered\n");
    let mut ctx = LintCtx::new(None);
    lint_workflow_contents(&wf, &mut ctx);
    note_skipped_capability_check(&mut ctx);
    let issues = ctx.issues;
    assert!(!has(&issues, Severity::Error, "unknown MCP"), "{issues:?}");
    assert!(
        has(&issues, Severity::Warning, "were not checked"),
        "{issues:?}"
    );
}

#[test]
fn flags_an_unregistered_agent_or_workflow_name() {
    let wf = parse_workflow_fixture("steps:\n  - agent: ghost\n  - workflow: phantom\n");
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(has(&issues, Severity::Error, "agent 'ghost'"), "{issues:?}");
    assert!(
        has(&issues, Severity::Error, "workflow 'phantom'"),
        "{issues:?}"
    );
}

#[test]
fn flags_a_missing_agent_file_or_child_workflow() {
    let wf = parse_workflow_fixture(
        "steps:\n  - agent: /nonexistent/agent-does-not-exist.md\n  - workflow: /nonexistent/child.yml\n",
    );
    let issues = lint_fixture(&wf, Some(&empty_config()));
    assert!(
        has(&issues, Severity::Error, "failed to load"),
        "{issues:?}"
    );
    assert!(
        has(&issues, Severity::Error, "could not be resolved"),
        "{issues:?}"
    );
}

#[test]
fn jq_referenced_fields_finds_dotted_and_bracketed_names() {
    assert_eq!(
        jq_referenced_fields("$inputs.a + $inputs[\"b\"] + ($inputs.a)", "$inputs"),
        vec!["a".to_owned(), "b".to_owned()]
    );
    assert!(jq_referenced_fields("$inputs | keys", "$inputs").is_empty());
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
        "---\noutput_schema:\n  type: object\nschema_name: \"bad name!\"\n---\nbody\n",
    );
    let report = lint_agent_file(&agent.path, Some(&empty_config()));
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.message.contains("JSON schema name")),
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
