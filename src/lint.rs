use std::{
    collections::HashSet,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};

use crate::{
    agent::{self, AgentFile},
    cli::LintArgs,
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
}

impl LintIssue {
    fn error(message: String) -> Self {
        Self {
            severity: Severity::Error,
            message,
        }
    }

    fn warning(message: String) -> Self {
        Self {
            severity: Severity::Warning,
            message,
        }
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

/// Runs `lait lint <FILES>...`: statically checks every file in
/// `lint_args.files` (see [`lint_file`]) and prints a per-file report to
/// stdout. Synchronous, like `history::run`/`session::run` — every check is
/// a local file read/parse, none of it needs the async runtime `app::run`
/// otherwise sets up for a model request (see `app::needs_async_runtime`).
/// Unlike `run_workflow`/`run_agent`, one bad file doesn't stop the rest:
/// every file is linted and reported before this returns `Err` (which only
/// happens if at least one file has an `Error`-level issue, so CI can rely
/// on the exit code).
pub(crate) fn run(lint_args: LintArgs, config_source: ConfigSource) -> Result<()> {
    // Unlike `config::load_config`, which returns an empty `ConfigFile` both
    // when `lait.config.yml` is absent/not found and when `--no-config` was
    // passed, the linter needs to tell "absent/skipped" apart from "present
    // but empty" so it can skip `mcp:`/`skills:` name checks (and say why)
    // instead of reporting every referenced name as unknown.
    let config_present = config::resolve_config_path(&config_source)?.is_some();
    let file_config = config::load_config(&config_source)?;
    let config = config_present.then_some(&file_config);

    let mut failed_files = 0usize;
    for file in &lint_args.files {
        // `lint_file` only ever returns `Err` for a file whose type it can't
        // determine (an unrecognized extension) — treated here as one more
        // failure to report, not a reason to stop linting the rest of
        // `lint_args.files`.
        let report = match lint_file(file, config) {
            Ok(report) => report,
            Err(error) => {
                println!("{}:", file.display());
                println!("  error: {error:#}");
                failed_files += 1;
                continue;
            }
        };
        if report.issues.is_empty() {
            println!("{}: OK", report.file.display());
            continue;
        }
        println!("{}:", report.file.display());
        for issue in &report.issues {
            println!("  {}: {}", issue.severity, issue.message);
        }
        if report.has_errors() {
            failed_files += 1;
        }
    }

    if failed_files > 0 {
        bail!(
            "{failed_files} of {} file(s) had errors",
            lint_args.files.len()
        );
    }
    Ok(())
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

fn lint_workflow_file(path: &Path, config: Option<&ConfigFile>) -> LintReport {
    let mut issues = Vec::new();
    let mut ctx = LintCtx::new(config);

    match workflow::load_workflow(path) {
        Err(error) => issues.push(LintIssue::error(format!("{error:#}"))),
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
        Err(error) => issues.push(LintIssue::error(format!("{error:#}"))),
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
            "'mcp'/'skills'/'subagents' names were not checked because no {} was found (or \
             --no-config was used)",
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

    if let Some(filter) = node.jq() {
        check_jq(filter, &format!("{node_context}: 'jq'"), issues);
    }
    check_mcp_names(&node_context, node.mcp(), ctx, issues);
    check_skill_names(&node_context, node.skills(), ctx, issues);
    check_subagent_names(&node_context, node.subagents(), ctx, issues);

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
}
