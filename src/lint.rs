//! `lait lint`: static validation of workflow YAML and agent Markdown files
//! without executing them.
//!
//! The lint rules themselves (`lint_workflow_file`/`lint_agent_file`,
//! `walk_steps`, `lint_node`, the `check_*` family) live in this file, since
//! they're the ones that actually read and write `LintIssue`/`LintReport`.
//! File discovery (which paths `lait lint <DIR>` walks into) is in
//! [`targets`]; the CLI-facing output layer (text/JSON/GitHub-Actions-
//! annotation formats) is in [`report`] — both only ever consume the
//! `LintIssue`/`LintReport`/`LintRun` vocabulary this file produces, never
//! the other way around.
//!
//! `lint_file` never fails on a bad workflow/agent file — a parse error or a
//! dangling reference becomes an `Error`-severity `LintIssue` in the
//! returned `LintReport` instead, so `lait lint <DIR>` can keep checking the
//! rest of a tree after one bad file.

use std::{
    collections::HashSet,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};

use crate::{
    agent::{self, AgentFile},
    cli::{LintArgs, LintFormat},
    config::{self, ConfigFile, ConfigSource},
    jq,
    nesting::{MAX_WORKFLOW_DEPTH, NestingDepthError, check_workflow_nesting},
    schema, template, workflow,
};

mod report;
mod targets;

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

/// One thing `lait lint` found wrong (or worth flagging) in a single file —
/// see `Severity` for the error/warning distinction.
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
    /// Whether any issue in this report is `Severity::Error` — the signal
    /// `lait lint`'s exit code and `lait run`/`lait agent run`'s upfront
    /// validation both key off of; a report containing only `Warning`
    /// issues does not fail either.
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
        let files = targets::expand_lint_targets(files)?;
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
}

/// Runs `lait lint`: expands `lint_args.files` into a concrete file list
/// (`LintRun::collect`), lints each one, and renders the combined report in
/// whichever format was requested. `report::run_text`/`report::run_structured`
/// print the full report either way, then `bail!` when `LintRun::has_errors`
/// is true — `main.rs`'s `is_lint` flag routes that error through
/// `error::classify`'s validation exit code rather than the general one, so
/// `lait lint`'s exit status reflects issue severity, not just success or
/// failure of the linting process itself.
pub(crate) fn run(lint_args: LintArgs, config_source: ConfigSource) -> Result<()> {
    let run = LintRun::collect(&lint_args.files, &config_source)?;
    match lint_args.format {
        LintFormat::Text => report::run_text(&run),
        LintFormat::Json | LintFormat::Github => report::run_structured(&run, lint_args.format),
    }
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
        if let Some(node_id) = step.node_id() {
            used.insert(node_id);
        }
        if let Some(when) = step.when() {
            check_jq(when, "a step's 'when'", issues);
        }
        if let Some(on_error) = step.on_error() {
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
                check_jq(
                    loop_def.condition.filter(),
                    &format!("a 'loop' step's '{}'", loop_def.condition.keyword()),
                    issues,
                );
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

/// Dispatches per-node-type checks after the checks that apply uniformly
/// across every node type (`jq`/`mcp`/`skills`/`subagents`/`tools`). Mirrors
/// `workflow::exec::nodes::execute`'s shape: one dispatcher, one function per
/// `workflow::NodeDefinition` variant.
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
            lint_prompt_node(node_id, &node_context, prompt, json_schemas, issues);
        }
        workflow::NodeDefinition::Agent(agent_node) => {
            lint_agent_node(node_id, agent_node, ctx, issues);
        }
        workflow::NodeDefinition::Workflow(workflow_node) => {
            lint_workflow_node(node_id, workflow_node, base_dir, ctx, issues, visited);
        }
        workflow::NodeDefinition::Command(command) => {
            lint_command_node(&node_context, command, issues);
        }
        workflow::NodeDefinition::Transform(_) => {}
        workflow::NodeDefinition::Ask(ask) => {
            check_prompt_template(&node_context, "'prompt' template", &ask.prompt, issues);
        }
    }
}

fn lint_prompt_node(
    node_id: &str,
    node_context: &str,
    prompt: &workflow::PromptNode,
    json_schemas: &schema::JsonSchemaMap,
    issues: &mut Vec<LintIssue>,
) {
    if let Some(template) = &prompt.prompt {
        check_prompt_template(node_context, "'prompt' template", template, issues);
    }
    if let Some(system_prompt) = &prompt.system_prompt {
        check_prompt_template(
            node_context,
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

fn lint_agent_node(
    node_id: &str,
    agent_node: &workflow::AgentNode,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
) {
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

fn lint_workflow_node(
    node_id: &str,
    workflow_node: &workflow::WorkflowNode,
    base_dir: &Path,
    ctx: &mut LintCtx,
    issues: &mut Vec<LintIssue>,
    visited: &mut Vec<PathBuf>,
) {
    lint_sub_workflow(
        node_id,
        &workflow_node.workflow,
        base_dir,
        ctx,
        issues,
        visited,
    );
}

fn lint_command_node(
    node_context: &str,
    command: &workflow::CommandNode,
    issues: &mut Vec<LintIssue>,
) {
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
        check_prompt_template(node_context, "'command' argument template", arg, issues);
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
mod tests;
