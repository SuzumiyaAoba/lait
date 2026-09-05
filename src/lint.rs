use std::{
    collections::HashSet,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    agent::{self, AgentFile},
    cli::{LintArgs, LintFormat},
    config::{self, ConfigFile, ConfigSource},
    jq,
    nesting::{MAX_WORKFLOW_DEPTH, NestingDepthError, check_workflow_nesting},
    schema, template, workflow,
};

/// How serious a `LintIssue` is. An `Error` names something that would fail
/// at `run`/`agent run` time (a bad reference, invalid syntax, a structural
/// mistake); a `Warning` names something that parses and would run, but is
/// probably not what the author meant (an unused node, a latent template
/// failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Error,
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        })
    }
}

#[derive(Debug)]
pub(crate) struct LintIssue {
    pub(crate) severity: Severity,
    pub(crate) message: String,
    /// The 1-based source line this issue was found at, when known. Only
    /// ever set directly for a YAML parse failure (from
    /// `serde_yaml::Error::location`, via `yaml_error_line`) — every other
    /// check site leaves this `None` at construction time. `--format
    /// json`/`--format github` fill in a best-effort line for those by
    /// searching the file's raw text for the first quoted identifier in the
    /// message (see `guess_line`), rather than threading a locator through
    /// every individual check.
    pub(crate) line: Option<usize>,
}

impl LintIssue {
    fn error(message: String) -> Self {
        Self {
            severity: Severity::Error,
            message,
            line: None,
        }
    }

    fn warning(message: String) -> Self {
        Self {
            severity: Severity::Warning,
            message,
            line: None,
        }
    }

    fn with_line(mut self, line: Option<usize>) -> Self {
        self.line = line;
        self
    }
}

/// Every issue found while linting a single file, in the order the checks
/// ran (structural/parse issues first, then reference/syntax checks).
#[derive(Debug)]
pub(crate) struct LintReport {
    pub(crate) file: PathBuf,
    pub(crate) issues: Vec<LintIssue>,
}

impl LintReport {
    pub(crate) fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == Severity::Error)
    }
}

/// Lints `path` without executing it: a workflow YAML file (`.yml`/`.yaml`)
/// or an agent Markdown file (`.md`), chosen by extension. `config` is
/// `Some` only when a `lait.config.yml` was actually found (or an explicit
/// one loaded); when `None`, `mcp:`/`skills:`/`subagents:` name references
/// are not checked (there is nothing to check them against) and the report
/// notes that instead of reporting every name as unknown.
///
/// This only ever returns `Err` for a file whose type can't be determined; a
/// file that fails to parse, or that references something that doesn't
/// exist, is reported as an `Error` issue in the returned `LintReport`
/// instead, so a caller linting many files can keep going after a bad one.
pub(crate) fn lint_file(path: &Path, config: Option<&ConfigFile>) -> Result<LintReport> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("yml") | Some("yaml") => Ok(lint_workflow_file(path, config)),
        Some("md") => Ok(lint_agent_file(path, config)),
        _ => bail!(
            "cannot determine the file type of '{}'; expected a '.yml'/'.yaml' workflow file or a '.md' agent file",
            path.display()
        ),
    }
}

/// Directory names `lait lint <DIR>` never descends into, even though they
/// don't start with `.` (dot-directories, e.g. `.git`, are always skipped
/// too) — scanning them would be slow, and their `.yml`/`.md` files
/// (dependency manifests, changelogs, CI configs belonging to a vendored
/// package, ...) are never lait workflow/agent files.
const SKIPPED_DIR_NAMES: &[&str] = &["target", "node_modules"];

/// Expands `paths` (files and/or directories, as `lait lint` accepts) into
/// the sorted, deduplicated list of files to actually lint: a file entry is
/// kept as-is (even one with an extension `lint_file` will go on to reject,
/// so that error is still reported per file); a directory entry is searched
/// recursively for `.yml`/`.yaml` files and `.md` files that start with a
/// `---` frontmatter delimiter (see `has_frontmatter_delimiter`), skipping
/// `SKIPPED_DIR_NAMES` and dot-directories along the way.
fn expand_lint_targets(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_lintable_files(path, &mut files)?;
        } else {
            files.push(path.clone());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_lintable_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory '{}'", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read directory '{}'", dir.display()))?;
    // Deterministic traversal order, so directory expansion is stable across
    // runs/platforms (relied on by tests, and generally friendlier for CI
    // diffs than filesystem-dependent order).
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect '{}'", path.display()))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIPPED_DIR_NAMES.contains(&name.as_ref()) {
                continue;
            }
            collect_lintable_files(&path, out)?;
        } else if file_type.is_file() {
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("yml") | Some("yaml") => out.push(path),
                Some("md") if has_frontmatter_delimiter(&path)? => out.push(path),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Cheaply sniffs whether `path` starts with the `---` frontmatter
/// delimiter `frontmatter::split` requires, without fully parsing it as an
/// agent file — only the first line is read. Used by directory expansion to
/// skip ordinary (non-agent) Markdown files like a README.
fn has_frontmatter_delimiter(path: &Path) -> Result<bool> {
    use std::io::BufRead;

    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let mut first_line = String::new();
    std::io::BufReader::new(file)
        .read_line(&mut first_line)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    Ok(first_line.trim_end_matches(['\n', '\r']) == "---")
}

/// Runs `lait lint <PATHS>...`: statically checks every file `expand_lint_targets`
/// resolves `lint_args.files` to (see [`lint_file`]) and reports the result
/// in `lint_args.format`. Synchronous, like `history::run`/`session::run` —
/// every check is a local file read/parse, none of it needs the async
/// runtime `app::run` otherwise sets up for a model request (see
/// `app::needs_async_runtime`). Unlike `run_workflow`/`run_agent`, one bad
/// file doesn't stop the rest: every file is linted and reported before
/// this returns `Err` (which only happens if at least one file has an
/// `Error`-level issue, so CI can rely on the exit code, regardless of
/// format).
/// A single analysis snapshot consumed by every output format.
/// Renderers never re-run checks, so adding a check cannot make formats disagree.
struct LintRun {
    config_display: String,
    registry: Vec<RegistryEntry>,
    api_key_errors: Vec<String>,
    tool_errors: Vec<String>,
    reports: Vec<LintReport>,
}

struct RegistryEntry {
    name: String,
    path: PathBuf,
    exists: bool,
}

impl LintRun {
    fn collect(files: &[PathBuf], config_source: &ConfigSource) -> Result<Self> {
        // An absent config skips capability checks; an existing empty config
        // must still reject unknown capability names.
        let config_path = config::resolve_config_path(config_source)?;
        let global_config_present = matches!(config_source, ConfigSource::Search)
            && config::global_config_path()?.is_file();
        let file_config = config::load_config(config_source)?;
        let config = (config_path.is_some() || global_config_present).then_some(&file_config);
        let files = expand_lint_targets(files)?;
        let mut registry: Vec<_> = file_config
            .workflows
            .iter()
            .map(|(name, path)| RegistryEntry {
                name: name.clone(),
                path: path.clone(),
                exists: path.is_file(),
            })
            .collect();
        registry.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let reports = files
            .iter()
            .map(|file| {
                lint_file(file, config).unwrap_or_else(|error| LintReport {
                    file: file.clone(),
                    issues: vec![LintIssue::error(format!("{error:#}"))],
                })
            })
            .collect();
        Ok(Self {
            config_display: config_path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| config::CONFIG_FILE_NAME.to_owned()),
            registry,
            api_key_errors: config::check_provider_api_key_sources(&file_config),
            tool_errors: config::check_shell_tool_definitions(&file_config),
            reports,
        })
    }

    fn failed_files(&self) -> usize {
        self.reports
            .iter()
            .filter(|report| report.has_errors())
            .count()
    }

    fn registry_ok(&self) -> bool {
        self.registry.iter().all(|entry| entry.exists)
    }

    fn has_errors(&self) -> bool {
        self.failed_files() > 0
            || !self.registry_ok()
            || !self.api_key_errors.is_empty()
            || !self.tool_errors.is_empty()
    }

    fn findings(&self) -> Vec<Finding> {
        let mut findings = Vec::new();
        for report in &self.reports {
            let mut source = None;
            for issue in &report.issues {
                let line = issue.line.or_else(|| {
                    let text = source.get_or_insert_with(|| {
                        std::fs::read_to_string(&report.file).unwrap_or_default()
                    });
                    guess_line(text, &issue.message)
                });
                findings.push(Finding {
                    file: report.file.display().to_string(),
                    line,
                    severity: issue.severity,
                    message: issue.message.clone(),
                });
            }
        }
        for entry in self.registry.iter().filter(|entry| !entry.exists) {
            findings.push(Finding::config(
                &self.config_display,
                Severity::Error,
                format!(
                    "workflows.{} resolves to '{}', which does not exist",
                    entry.name,
                    entry.path.display()
                ),
            ));
        }
        for message in self.api_key_errors.iter().chain(&self.tool_errors) {
            findings.push(Finding::config(
                &self.config_display,
                Severity::Error,
                message.clone(),
            ));
        }
        findings
    }
}

pub(crate) fn run(lint_args: LintArgs, config_source: ConfigSource) -> Result<()> {
    let run = LintRun::collect(&lint_args.files, &config_source)?;
    match lint_args.format {
        LintFormat::Text => run_text(&run),
        LintFormat::Json | LintFormat::Github => run_structured(&run, lint_args.format),
    }
}

fn run_text(run: &LintRun) -> Result<()> {
    if !run.registry.is_empty() {
        println!("{} (workflows:):", config::CONFIG_FILE_NAME);
        for entry in &run.registry {
            if entry.exists {
                println!("  {}: OK ({})", entry.name, entry.path.display());
            } else {
                println!(
                    "  {}: error: no such file '{}'",
                    entry.name,
                    entry.path.display()
                );
            }
        }
    }
    print_config_errors("api_key/api_key_cmd:", &run.api_key_errors);
    print_config_errors("tools:", &run.tool_errors);
    for report in &run.reports {
        if report.issues.is_empty() {
            println!("{}: OK", report.file.display());
        } else {
            println!("{}:", report.file.display());
            for issue in &report.issues {
                println!("  {}: {}", issue.severity, issue.message);
            }
        }
    }
    if run.has_errors() {
        let mut suffix = String::new();
        for (ok, section) in [
            (run.registry_ok(), "'workflows:'"),
            (run.api_key_errors.is_empty(), "api_key/api_key_cmd"),
            (run.tool_errors.is_empty(), "'tools:'"),
        ] {
            if !ok {
                suffix.push_str(&format!(
                    "; {} {section} also has errors",
                    config::CONFIG_FILE_NAME
                ));
            }
        }
        bail!(
            "{} of {} file(s) had errors{suffix}",
            run.failed_files(),
            run.reports.len()
        );
    }
    Ok(())
}

/// One machine-readable finding attributed to a file and optional source line.
struct Finding {
    file: String,
    line: Option<usize>,
    severity: Severity,
    message: String,
}

impl Finding {
    fn config(config_display: &str, severity: Severity, message: String) -> Self {
        Self {
            file: config_display.to_owned(),
            line: None,
            severity,
            message,
        }
    }
}

fn run_structured(run: &LintRun, format: LintFormat) -> Result<()> {
    let findings = run.findings();
    match format {
        LintFormat::Json => print_json_findings(&findings)?,
        LintFormat::Github => print_github_findings(&findings),
        LintFormat::Text => unreachable!("text has its own renderer"),
    }
    if run.has_errors() {
        bail!(
            "lint found {} error(s) across {} finding(s) in {} file(s)",
            findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error)
                .count(),
            findings.len(),
            run.reports.len(),
        );
    }
    Ok(())
}

fn print_config_errors(section: &str, errors: &[String]) {
    if !errors.is_empty() {
        println!("{} ({section}):", config::CONFIG_FILE_NAME);
        for error in errors {
            println!("  error: {error}");
        }
    }
}

fn print_json_findings(findings: &[Finding]) -> Result<()> {
    let records: Vec<serde_json::Value> = findings
        .iter()
        .map(|finding| {
            serde_json::json!({
                "file": finding.file,
                "line": finding.line,
                "severity": match finding.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                },
                "message": finding.message,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&records).context("failed to serialize lint findings")?
    );
    Ok(())
}

fn print_github_findings(findings: &[Finding]) {
    for finding in findings {
        let level = match finding.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let message = escape_github_annotation(&finding.message);
        match finding.line {
            Some(line) => println!("::{level} file={},line={line}::{message}", finding.file),
            None => println!("::{level} file={}::{message}", finding.file),
        }
    }
}

/// Escapes a message for a GitHub Actions workflow command
/// (`::error ...::<message>`), per GitHub's documented `%`/CR/LF escaping —
/// otherwise a message containing one of these could corrupt the annotation
/// or be misread as a second command.
fn escape_github_annotation(message: &str) -> String {
    message
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Best-effort line lookup for an issue that has no line of its own (i.e.
/// everything except a YAML parse failure — see `yaml_error_line`): most
/// lint messages name the offending thing in single quotes (`node 'x'`,
/// `unknown MCP server 'y'`, ...), which is usually also how it appears
/// literally in the source (a YAML mapping key, a list entry, ...). Returns
/// the 1-based line of the first line containing that quoted text, or `None`
/// when the message has no quoted identifier or nothing in `source` matches
/// it. A heuristic, not a real position — good enough for an editor/CI
/// annotation to land a reader in the right neighborhood, not a guarantee.
fn guess_line(source: &str, message: &str) -> Option<usize> {
    let needle = first_quoted_identifier(message)?;
    source
        .lines()
        .position(|line| line.contains(needle))
        .map(|index| index + 1)
}

fn first_quoted_identifier(message: &str) -> Option<&str> {
    let start = message.find('\'')? + 1;
    let end = message[start..].find('\'')?;
    let candidate = &message[start..start + end];
    (!candidate.is_empty()).then_some(candidate)
}

/// Threaded through every check in one `lint_file` call: `config` is looked
/// up by every `mcp:`/`skills:` name check, and `skipped_capability_check` is
/// set the first time one of those checks has no `config` to check against,
/// so the report can note it once rather than repeat the same caveat next to
/// every name.
struct LintCtx<'a> {
    config: Option<&'a ConfigFile>,
    skipped_capability_check: bool,
}

impl<'a> LintCtx<'a> {
    fn new(config: Option<&'a ConfigFile>) -> Self {
        Self {
            config,
            skipped_capability_check: false,
        }
    }
}

/// The 1-based line a YAML parse failure happened at, when `error`'s chain
/// contains a `serde_yaml::Error` that reports one (see
/// `serde_yaml::Error::location`). Unlike `guess_line`'s message-text
/// heuristic (used for every other kind of issue), this is an exact
/// position straight from the parser.
fn yaml_error_line(error: &anyhow::Error) -> Option<usize> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<serde_yaml::Error>())
        .and_then(|error| error.location())
        .map(|location| location.line())
}

fn lint_workflow_file(path: &Path, config: Option<&ConfigFile>) -> LintReport {
    let mut issues = Vec::new();
    let mut ctx = LintCtx::new(config);

    match workflow::load_workflow(path) {
        Err(error) => {
            let line = yaml_error_line(&error);
            issues.push(LintIssue::error(format!("{error:#}")).with_line(line));
        }
        Ok(wf) => {
            // Seeded with this file's own canonical path so a `workflow:`
            // chain that loops back to it is caught the same way
            // `WorkflowScope::nested` catches it at `run` time.
            let mut visited = Vec::new();
            let canonical = std::fs::canonicalize(path).ok();
            // Runtime resolves nested workflow paths from the canonical
            // top-level file's parent (`WorkflowScope::top_level`).  Keep
            // linting on that same base so invoking lint through a symlink
            // cannot inspect a different set of relative sub-workflows than
            // `run` would execute.
            let base_dir = canonical
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .or_else(|| path.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."));
            if let Some(canonical) = canonical {
                visited.push(canonical);
            }
            lint_workflow_contents(&wf, &base_dir, &mut ctx, &mut issues, &mut visited);
        }
    }

    note_skipped_capability_check(&mut ctx, &mut issues);
    LintReport {
        file: path.to_path_buf(),
        issues,
    }
}

fn lint_agent_file(path: &Path, config: Option<&ConfigFile>) -> LintReport {
    let mut issues = Vec::new();
    let mut ctx = LintCtx::new(config);

    match agent::load_agent(path) {
        Err(error) => {
            let line = yaml_error_line(&error);
            issues.push(LintIssue::error(format!("{error:#}")).with_line(line));
        }
        Ok(agent_file) => lint_agent_contents("the agent", &agent_file, &mut ctx, &mut issues),
    }

    note_skipped_capability_check(&mut ctx, &mut issues);
    LintReport {
        file: path.to_path_buf(),
        issues,
    }
}

fn note_skipped_capability_check(ctx: &mut LintCtx, issues: &mut Vec<LintIssue>) {
    if ctx.skipped_capability_check {
        issues.push(LintIssue::warning(format!(
            "'mcp'/'skills'/'subagents'/'tools' names were not checked because no {} was found \
             (or --no-config was used)",
            config::CONFIG_FILE_NAME
        )));
    }
}

/// Walks one already-loaded workflow's `nodes:`/`steps:` looking for
/// problems `workflow::load_workflow` doesn't already catch: nodes that are
/// defined but never used, and references (`agent:`/`workflow:`/`mcp:`/
/// `skills:`/schema names/jq filters/templates) that would only fail lazily,
/// at `run` time. Recurses into every `workflow:` node's sub-workflow file
/// (resolved against `base_dir`, the directory `wf`'s own file lives in) and
/// every `agent:` node's agent file.
fn lint_workflow_contents(
    wf: &workflow::WorkflowFile,
    base_dir: &Path,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
    visited: &mut Vec<PathBuf>,
) {
    let mut used_node_ids = HashSet::new();
    walk_steps(&wf.steps, &mut used_node_ids, issues);
    for node_id in wf.nodes.keys() {
        if !used_node_ids.contains(node_id.as_str()) {
            issues.push(LintIssue::warning(format!(
                "node '{node_id}' is defined in 'nodes:' but never referenced by a step's 'use'"
            )));
        }
    }

    check_mcp_names(
        "the workflow's 'default'",
        wf.default.mcp.as_deref(),
        ctx,
        issues,
    );
    check_skill_names(
        "the workflow's 'default'",
        wf.default.skills.as_deref(),
        ctx,
        issues,
    );
    check_subagent_names(
        "the workflow's 'default'",
        wf.default.subagents.as_deref(),
        ctx,
        issues,
    );
    check_tool_names(
        "the workflow's 'default'",
        wf.default.tools.as_deref(),
        ctx,
        issues,
    );
    if let Some(system_prompt) = &wf.default.system_prompt {
        check_prompt_template(
            "the workflow's 'default'",
            "'system_prompt' template",
            system_prompt,
            issues,
        );
    }

    for (node_id, node) in &wf.nodes {
        lint_node(
            node_id,
            node,
            base_dir,
            &wf.json_schemas,
            ctx,
            issues,
            visited,
        );
    }
}

/// Records every `use:` id reached from `steps` (recursing into `on_error`
/// and every router kind's nested `steps`/`cases`/`branches`) into `used`,
/// and checks every jq filter site (`when`, `switch` case `when`s, `loop`
/// `while`/`until`, `for_each` `items`, `parallel`/`for_each` `join`) for
/// syntax errors along the way. Mirrors the tree `workflow::validate_steps`
/// walks, so a router kind added there needs updating here too.
fn walk_steps<'a>(
    steps: &'a [workflow::FlowStep],
    used: &mut HashSet<&'a str>,
    issues: &mut Vec<LintIssue>,
) {
    for step in steps {
        if let Some(node_id) = &step.r#use {
            used.insert(node_id.as_str());
        }
        if let Some(when) = &step.when {
            check_jq(when, "a step's 'when'", issues);
        }
        if let Some(on_error) = &step.on_error {
            walk_steps(&on_error.steps, used, issues);
        }

        // Matched exhaustively (no `_` arm), like `validate_steps`' and
        // `run_steps`' own matches on this same enum, so a new router kind
        // fails to compile here until this function's traversal is updated
        // for it too.
        match step.router() {
            Some(workflow::Router::Switch(switch)) => {
                for case in &switch.cases {
                    check_jq(&case.when, "a 'switch' case's 'when'", issues);
                    walk_steps(&case.steps, used, issues);
                }
                if let Some(else_steps) = &switch.else_steps {
                    walk_steps(else_steps, used, issues);
                }
            }
            Some(workflow::Router::Parallel(parallel)) => {
                for branch in &parallel.branches {
                    walk_steps(&branch.steps, used, issues);
                }
                if let Some(join) = &parallel.join {
                    check_jq(join, "a 'parallel' step's 'join'", issues);
                }
            }
            Some(workflow::Router::Loop(loop_def)) => {
                if let Some(condition) = &loop_def.r#while {
                    check_jq(condition, "a 'loop' step's 'while'", issues);
                }
                if let Some(condition) = &loop_def.until {
                    check_jq(condition, "a 'loop' step's 'until'", issues);
                }
                walk_steps(&loop_def.steps, used, issues);
            }
            Some(workflow::Router::ForEach(for_each)) => {
                check_jq(&for_each.items, "a 'for_each' step's 'items'", issues);
                walk_steps(&for_each.steps, used, issues);
                if let Some(join) = &for_each.join {
                    check_jq(join, "a 'for_each' step's 'join'", issues);
                }
            }
            None => {}
        }
    }
}

fn check_jq(filter: &str, description: &str, issues: &mut Vec<LintIssue>) {
    if let Err(error) = jq::check_syntax(filter) {
        issues.push(LintIssue::error(format!(
            "{description} has an invalid jq filter {filter:?}: {error:#}"
        )));
    }
}

#[allow(clippy::too_many_arguments)]
fn lint_node(
    node_id: &str,
    node: &workflow::NodeDefinition,
    base_dir: &Path,
    json_schemas: &schema::JsonSchemaMap,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
    visited: &mut Vec<PathBuf>,
) {
    let node_context = format!("node '{node_id}'");
    let settings = node.settings();

    if let Some(filter) = settings.jq {
        check_jq(filter, &format!("{node_context}: 'jq'"), issues);
    }
    check_mcp_names(&node_context, settings.mcp, ctx, issues);
    check_skill_names(&node_context, settings.skills, ctx, issues);
    check_subagent_names(&node_context, settings.subagents, ctx, issues);
    check_tool_names(&node_context, settings.tools, ctx, issues);

    match node {
        workflow::NodeDefinition::Prompt(prompt) => {
            if let Some(template) = &prompt.prompt {
                check_prompt_template(&node_context, "'prompt' template", template, issues);
            }
            if let Some(system_prompt) = &prompt.system_prompt {
                check_prompt_template(
                    &node_context,
                    "'system_prompt' template",
                    system_prompt,
                    issues,
                );
            }
            if let Some(name_or_path) = &prompt.input_schema {
                match schema::resolve_named_schema_value(json_schemas, name_or_path) {
                    Ok(resolved) => check_unrecognized_schema_types(
                        &format!("node '{node_id}''s 'input_schema'"),
                        &resolved,
                        issues,
                    ),
                    Err(error) => issues.push(LintIssue::error(format!(
                        "node '{node_id}' has an unresolvable 'input_schema': {error:#}"
                    ))),
                }
            }
            if let Some(name_or_path) = &prompt.output_schema {
                if let Err(error) = schema::resolve_named_schema_value(json_schemas, name_or_path) {
                    issues.push(LintIssue::error(format!(
                        "node '{node_id}' has an unresolvable 'output_schema': {error:#}"
                    )));
                }
                // `output_schema` implies a `schema_name` (the node's own, or
                // the "structured_output" default) is sent as the Structured
                // Outputs request's schema name — validated only at request
                // time otherwise (see `schema::build_json_schema`).
                let schema_name = prompt.schema_name.as_deref().unwrap_or("structured_output");
                if let Err(error) = schema::validate_schema_name(schema_name) {
                    issues.push(LintIssue::error(format!(
                        "node '{node_id}' has an invalid 'schema_name': {error:#}"
                    )));
                }
            }
        }
        workflow::NodeDefinition::Agent(agent_node) => {
            // Matches `execute_step`: `agent:` is loaded as given, relative
            // to the current working directory (unlike `workflow:`, which
            // resolves against the workflow file's own directory) — see
            // `AgentNode::agent`'s doc comment.
            match agent::load_agent(&agent_node.agent) {
                Ok(agent_file) => lint_agent_contents(
                    &format!("node '{node_id}''s agent"),
                    &agent_file,
                    ctx,
                    issues,
                ),
                Err(error) => issues.push(LintIssue::error(format!(
                    "node '{node_id}' has 'agent: {}' (resolved relative to the current working \
                     directory, not this workflow file), which failed to load: {error:#}",
                    agent_node.agent.display()
                ))),
            }
        }
        workflow::NodeDefinition::Workflow(workflow_node) => {
            lint_sub_workflow(
                node_id,
                &workflow_node.workflow,
                base_dir,
                ctx,
                issues,
                visited,
            );
        }
        workflow::NodeDefinition::Command(command) => {
            if command
                .command
                .first()
                .is_some_and(|program| program.trim().is_empty())
            {
                issues.push(LintIssue::error(format!(
                    "{node_context} has an empty 'command[0]' program; it must name an executable"
                )));
            }
            for arg in &command.command {
                check_prompt_template(&node_context, "'command' argument template", arg, issues);
            }
        }
        workflow::NodeDefinition::Transform(_) => {}
        workflow::NodeDefinition::Ask(ask) => {
            check_prompt_template(&node_context, "'prompt' template", &ask.prompt, issues);
        }
    }
}

/// Checks a `prompt`/system-prompt-template's handlebars syntax. `field`
/// names the source in the message (`"'prompt' template"` for a node,
/// `"system prompt template"` for an agent file — agents have no `prompt:`,
/// their template is the Markdown body, so sharing one label between the two
/// would misname it for one of them).
fn check_prompt_template(
    context: &str,
    field: &str,
    template_source: &str,
    issues: &mut Vec<LintIssue>,
) {
    if let Err(error) = template::check_syntax(template_source) {
        issues.push(LintIssue::error(format!(
            "{context} has an invalid {field}: {error:#}"
        )));
    }
}

/// Checks an agent file's `input_schema`/`output_schema` entry (an inline
/// schema or a file path — see `schema::load_schema_value`), shared since
/// both fields are checked identically, only differing in `field`'s name in
/// the issue's message.
fn check_schema_entry(
    context: &str,
    field: &str,
    entry: Option<&schema::JsonSchemaEntry>,
    issues: &mut Vec<LintIssue>,
) {
    let Some(entry) = entry else { return };
    match schema::load_schema_value(entry) {
        // Only `input_schema` is actually checked against the runtime input
        // locally (`schema::validate_input_against_schema`) — `output_schema`
        // is sent to the model as-is for Structured Outputs, so an
        // unrecognized `type` there is the server's problem, not lait's.
        Ok(resolved) if field == "input_schema" => {
            check_unrecognized_schema_types(&format!("{context}'s '{field}'"), &resolved, issues);
        }
        Ok(_) => {}
        Err(error) => issues.push(LintIssue::error(format!(
            "{context}'s '{field}' is invalid: {error:#}"
        ))),
    }
}

/// Warns about every `type` keyword value in `schema` that isn't one of
/// JSON Schema's recognized primitive type names (`schema::
/// unrecognized_type_names`) — `validate_input_against_schema` silently
/// treats an unrecognized name as matching any value, so a typo like
/// `type: sting` would otherwise leave that field completely unchecked
/// without any indication why.
fn check_unrecognized_schema_types(
    context: &str,
    schema: &serde_json::Value,
    issues: &mut Vec<LintIssue>,
) {
    for type_name in schema::unrecognized_type_names(schema) {
        issues.push(LintIssue::warning(format!(
            "{context} uses 'type: {type_name}', which is not a JSON Schema type lait recognizes; \
             it will not be enforced (treated as matching any value)"
        )));
    }
}

fn lint_sub_workflow(
    node_id: &str,
    sub_workflow_path: &Path,
    base_dir: &Path,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
    visited: &mut Vec<PathBuf>,
) {
    let resolved = base_dir.join(sub_workflow_path);
    let canonical = match std::fs::canonicalize(&resolved) {
        Ok(canonical) => canonical,
        Err(error) => {
            issues.push(LintIssue::error(format!(
                "node '{node_id}' has 'workflow: {}', which could not be resolved: {error}",
                sub_workflow_path.display()
            )));
            return;
        }
    };
    // Shares `WorkflowScope::nested`'s cycle/depth-cap check, so a
    // non-cyclic-but-arbitrarily-deep or cyclic `workflow:` chain is flagged
    // here the same way it would fail at `run` time.
    if let Err(error) = check_workflow_nesting(visited, &canonical) {
        issues.push(LintIssue::error(match error {
            NestingDepthError::Cycle => format!(
                "node '{node_id}' has 'workflow: {}', which would create a cycle ('{}' is \
                 already being linted)",
                sub_workflow_path.display(),
                canonical.display()
            ),
            NestingDepthError::TooDeep => format!(
                "node '{node_id}' has 'workflow: {}', which exceeds the maximum 'workflow:' \
                 nesting depth of {MAX_WORKFLOW_DEPTH}",
                sub_workflow_path.display()
            ),
        }));
        return;
    }

    match workflow::load_workflow(&resolved) {
        Err(error) => issues.push(LintIssue::error(format!(
            "node '{node_id}' has 'workflow: {}', which failed to load: {error:#}",
            sub_workflow_path.display()
        ))),
        Ok(sub_wf) => {
            let sub_base_dir = canonical
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            visited.push(canonical);
            // `lint_workflow_contents` pushes straight into `issues`, so
            // without this, a message from `sub_workflow_path`'s own
            // 'nodes:'/'steps:' (e.g. an unused-node warning, whose node ids
            // are only unique within their own file) would print under the
            // top-level file's header with nothing saying which file it
            // actually came from. Prefix every message this recursive call
            // adds with the sub-workflow's path to attribute it.
            let issues_before = issues.len();
            lint_workflow_contents(&sub_wf, &sub_base_dir, ctx, issues, visited);
            for issue in &mut issues[issues_before..] {
                issue.message = format!(
                    "in 'workflow: {}': {}",
                    sub_workflow_path.display(),
                    issue.message
                );
            }
            visited.pop();
        }
    }
}

/// Checks the parts of an agent file that `agent::load_agent` doesn't
/// already validate: its system prompt template's handlebars syntax, its
/// `input_schema`/`output_schema` (when set as an inline schema or a file
/// path, whichever resolves without error), and its `mcp:`/`skills:` names.
/// `context` names where this agent file came from in a lint message (e.g.
/// `"the agent"` for a top-level `agent run`/`agent lint` target, or `"node
/// 'x''s agent"` for a workflow node's `agent:`).
fn lint_agent_contents(
    context: &str,
    agent_file: &AgentFile,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
) {
    check_prompt_template(
        context,
        "system prompt template",
        &agent_file.system_prompt_template,
        issues,
    );

    check_schema_entry(
        context,
        "input_schema",
        agent_file.input_schema.as_ref(),
        issues,
    );
    check_schema_entry(
        context,
        "output_schema",
        agent_file.output_schema.as_ref(),
        issues,
    );
    // `structured_output: true` requires `output_schema` (checked at parse
    // time by `agent::parse_agent`), so this is reached only when a
    // `schema_name` (the agent's own, or the "structured_output" default) is
    // actually sent as the Structured Outputs request's schema name — see
    // the matching check in `lint_node`.
    if agent_file.structured_output
        && let Err(error) = schema::validate_schema_name(agent_file.schema_name())
    {
        issues.push(LintIssue::error(format!(
            "{context} has an invalid 'schema_name': {error:#}"
        )));
    }

    check_mcp_names(context, agent_file.mcp.as_deref(), ctx, issues);
    check_skill_names(context, agent_file.skills.as_deref(), ctx, issues);
    check_subagent_names(context, agent_file.subagents.as_deref(), ctx, issues);
    check_tool_names(context, agent_file.tools.as_deref(), ctx, issues);
}

fn check_mcp_names(
    context: &str,
    names: Option<&[String]>,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
) {
    check_capability_names(
        context,
        "MCP server",
        "mcp_servers:",
        names,
        |config, name| config.mcp_servers.contains_key(name),
        ctx,
        issues,
    );
    check_mcp_allowed_tools_not_empty(context, names, ctx, issues);
}

/// Warns when a node/agent references an MCP server whose `allowed_tools`
/// (see `McpRegistry::call`) is an explicit empty list — every tool call to
/// it is unconditionally rejected at runtime, so referencing such a server
/// at all is almost certainly a mistake. Distinct from
/// `check_capability_names`'s unknown-name check above: this only fires for
/// servers that *do* exist, and is a warning (not an error) since lait
/// cannot know in advance which tool, if any, the model will actually try
/// to call — an `allowed_tools` list that is merely non-empty could still
/// reject some calls at runtime with no way to tell in advance.
fn check_mcp_allowed_tools_not_empty(
    context: &str,
    names: Option<&[String]>,
    ctx: &LintCtx,
    issues: &mut Vec<LintIssue>,
) {
    let Some(names) = names else { return };
    let Some(config) = ctx.config else { return };
    for name in names {
        if let Some(server) = config.mcp_servers.get(name)
            && let Some(allowed_tools) = &server.allowed_tools
            && allowed_tools.is_empty()
        {
            issues.push(LintIssue::warning(format!(
                "{context} references MCP server '{name}', whose 'allowed_tools' in {} is an empty list; every tool call to it will be rejected at runtime",
                config::CONFIG_FILE_NAME
            )));
        }
    }
}

fn check_skill_names(
    context: &str,
    names: Option<&[String]>,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
) {
    check_capability_names(
        context,
        "skill",
        "skills:",
        names,
        |config, name| config.skills.contains_key(name),
        ctx,
        issues,
    );
}

fn check_subagent_names(
    context: &str,
    names: Option<&[String]>,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
) {
    check_capability_names(
        context,
        "subagent",
        "agents:",
        names,
        |config, name| config.agents.contains_key(name),
        ctx,
        issues,
    );
}

fn check_tool_names(
    context: &str,
    names: Option<&[String]>,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
) {
    check_capability_names(
        context,
        "tool",
        "tools:",
        names,
        |config, name| config.tools.contains_key(name),
        ctx,
        issues,
    );
}

/// Shared by `check_mcp_names`/`check_skill_names`: both look up a list of
/// names against a map defined in `config` (`skipping`, and noting once, when
/// there is no `config` to check against at all), differing only in which map
/// they check and how they name it in an issue's message. `contains` decides
/// whether a name is defined (`|config, name| config.mcp_servers...`/
/// `config.skills...`); `field` is the `lait.config.yml` key to point at.
fn check_capability_names(
    context: &str,
    kind: &str,
    field: &str,
    names: Option<&[String]>,
    contains: impl Fn(&ConfigFile, &str) -> bool,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
) {
    let Some(names) = names else { return };
    if names.is_empty() {
        return;
    }
    let Some(config) = ctx.config else {
        ctx.skipped_capability_check = true;
        return;
    };
    for name in names {
        if !contains(config, name) {
            issues.push(LintIssue::error(format!(
                "{context} references unknown {kind} '{name}'; define it under '{field}' in {}",
                config::CONFIG_FILE_NAME
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse_workflow_fixture(yaml: &str) -> workflow::WorkflowFile {
        serde_yaml::from_str(yaml).expect("fixture workflow should deserialize")
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
            issues.iter().any(
                |issue| issue.severity == Severity::Warning && issue.message.contains("unused")
            ),
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
    fn first_quoted_identifier_extracts_the_first_single_quoted_span() {
        assert_eq!(
            first_quoted_identifier("node 'extract': unknown skill 'nope'"),
            Some("extract")
        );
    }

    #[test]
    fn first_quoted_identifier_is_none_without_quotes() {
        assert_eq!(first_quoted_identifier("no quotes here"), None);
    }

    #[test]
    fn guess_line_finds_the_line_containing_the_quoted_identifier() {
        let source = "nodes:\n  extract:\n    type: prompt\n    prompt: hi\n";
        assert_eq!(guess_line(source, "node 'extract' is unused"), Some(2));
    }

    #[test]
    fn guess_line_is_none_when_nothing_matches() {
        let source = "nodes:\n  extract:\n    type: prompt\n";
        assert_eq!(guess_line(source, "node 'missing' is unused"), None);
    }

    #[test]
    fn has_frontmatter_delimiter_detects_agent_style_files() {
        crate::test_support::in_temp_dir("lait-test-lint-frontmatter", || {
            std::fs::write("agent.md", "---\nname: x\n---\nbody\n").unwrap();
            std::fs::write("plain.md", "# Just a heading\n\nbody\n").unwrap();

            assert!(has_frontmatter_delimiter(Path::new("agent.md")).unwrap());
            assert!(!has_frontmatter_delimiter(Path::new("plain.md")).unwrap());
        });
    }

    #[test]
    fn expand_lint_targets_recurses_into_directories_and_skips_non_agent_markdown() {
        crate::test_support::in_temp_dir("lait-test-lint-expand", || {
            std::fs::create_dir_all("sub").unwrap();
            std::fs::write("sub/workflow.yml", "steps: []\n").unwrap();
            std::fs::write("sub/agent.md", "---\n---\nbody\n").unwrap();
            std::fs::write("sub/README.md", "# not an agent file\n").unwrap();
            std::fs::write("sub/notes.txt", "irrelevant\n").unwrap();

            let files = expand_lint_targets(&[PathBuf::from(".")]).unwrap();

            assert_eq!(
                files,
                vec![
                    PathBuf::from("./sub/agent.md"),
                    PathBuf::from("./sub/workflow.yml"),
                ]
            );
        });
    }

    #[test]
    fn expand_lint_targets_skips_target_and_node_modules_and_dot_directories() {
        crate::test_support::in_temp_dir("lait-test-lint-expand-skip", || {
            std::fs::write("top.yml", "steps: []\n").unwrap();
            std::fs::create_dir_all("target").unwrap();
            std::fs::write("target/build.yml", "steps: []\n").unwrap();
            std::fs::create_dir_all("node_modules/pkg").unwrap();
            std::fs::write("node_modules/pkg/ci.yml", "steps: []\n").unwrap();
            std::fs::create_dir_all(".git").unwrap();
            std::fs::write(".git/config.yml", "steps: []\n").unwrap();

            let files = expand_lint_targets(&[PathBuf::from(".")]).unwrap();

            assert_eq!(files, vec![PathBuf::from("./top.yml")]);
        });
    }

    #[test]
    fn expand_lint_targets_passes_through_explicit_files_unchanged() {
        let files = expand_lint_targets(&[PathBuf::from("a.yml"), PathBuf::from("b.md")]).unwrap();
        assert_eq!(files, vec![PathBuf::from("a.yml"), PathBuf::from("b.md")]);
    }

    #[test]
    fn yaml_error_line_reports_the_parser_location() {
        let error = serde_yaml::from_str::<workflow::WorkflowFile>("steps: [\n")
            .expect_err("malformed YAML should fail to parse");
        let line = yaml_error_line(&anyhow::Error::new(error));
        assert!(line.is_some(), "expected a line number from the parser");
    }
}
