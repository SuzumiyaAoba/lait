//! `lait lint`: static validation of workflow YAML and agent Markdown files
//! without executing them.
//!
//! `lint_file`'s two entry points split by what they lint: workflow-YAML
//! rules (`lint_workflow_file`, `walk_steps`, `lint_node`, most of the
//! `check_*` family) live in [`workflow_lint`]; agent-Markdown rules
//! (`lint_agent_file`/`lint_agent_contents`) and the capability-name checks
//! both share (`check_capability_name_lists`/`check_capability_names`/
//! `check_mcp_allowed_tools_not_empty`) stay in this file, along with
//! `LintCtx` (the state threaded through every check in one `lint_file`
//! call, in both modules) and `LintIssue`/`LintReport` themselves. File
//! discovery (which paths `lait lint <DIR>` walks into) is in [`targets`];
//! the CLI-facing output layer (text/JSON/GitHub-Actions-annotation
//! formats) is in [`report`] — both only ever consume the
//! `LintIssue`/`LintReport`/`LintRun` vocabulary this file produces, never
//! the other way around.
//!
//! `lint_file` never fails on a bad workflow/agent file — a parse error or a
//! dangling reference becomes an `Error`-severity `LintIssue` in the
//! returned `LintReport` instead, so `lait lint <DIR>` can keep checking the
//! rest of a tree after one bad file.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    rc::Rc,
};

use anyhow::{Result, bail};

use crate::{
    agent::{self, AgentFile},
    cli::{LintArgs, LintFormat},
    config::{self, ConfigFile, ConfigSource},
    schema, workflow,
};

mod report;
mod targets;
mod workflow_lint;
// `lint_workflow_file` is called directly by `lint_file` below; the rest
// (`check_prompt_template`/`check_schema_entry`/`yaml_error_line`, each also
// called from this file's own `lint_agent_file`/`lint_agent_contents`, plus
// everything `lint/tests.rs` reaches through its `use super::*;`) is
// re-exported here rather than qualified as `workflow_lint::` at every call
// site.
use workflow_lint::*;

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

/// Lints every file in `files` (each an independent `lint_file` call — no
/// shared mutable state, since each gets its own fresh `LintCtx`), spread
/// across a small, bounded set of OS threads instead of one at a time on the
/// calling thread. `lint` has no async runtime to reach for (see `run`'s doc
/// comment), and the file discovery/parsing this does is exactly the CPU +
/// disk-read shaped workload `std::thread::scope` suits: no futures runtime
/// needed, and results are joined and returned before this function returns,
/// so no thread outlives the scope.
///
/// Files are split into contiguous chunks (not one thread per file) so a
/// `lait lint` over a directory of hundreds of files doesn't spawn hundreds
/// of OS threads; each chunk is linted sequentially by its own thread, and
/// chunk order matches `files`' order, so the returned reports stay in the
/// same order `files` came in (matching the previous sequential behavior,
/// which callers/tests rely on for stable output).
fn lint_files_concurrently(files: &[PathBuf], config: Option<&ConfigFile>) -> Vec<LintReport> {
    if files.is_empty() {
        return Vec::new();
    }

    let worker_count = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(files.len());
    let chunk_size = files.len().div_ceil(worker_count);

    fn lint_chunk(chunk: &[PathBuf], config: Option<&ConfigFile>) -> Vec<LintReport> {
        chunk
            .iter()
            .map(|file| {
                lint_file(file, config).unwrap_or_else(|error| LintReport {
                    file: file.clone(),
                    issues: vec![LintIssue::error(format!("{error:#}"))],
                })
            })
            .collect()
    }

    std::thread::scope(|scope| {
        let handles: Vec<(&[PathBuf], std::thread::ScopedJoinHandle<Vec<LintReport>>)> = files
            .chunks(chunk_size)
            .map(|chunk| (chunk, scope.spawn(move || lint_chunk(chunk, config))))
            .collect();
        handles
            .into_iter()
            .flat_map(|(chunk, handle)| {
                handle.join().unwrap_or_else(|_| {
                    // `lint_file` doesn't panic under normal operation, but a
                    // worker thread panic must not silently drop every
                    // report the rest of its chunk would have produced.
                    chunk
                        .iter()
                        .map(|file| LintReport {
                            file: file.clone(),
                            issues: vec![LintIssue::error(
                                "internal error: the lint worker thread for this file panicked"
                                    .to_owned(),
                            )],
                        })
                        .collect()
                })
            })
            .collect()
    })
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
        let reports = lint_files_concurrently(&files, config);
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

/// Threaded through every check in one `lint_file` call. `config` is looked
/// up by every `mcp:`/`skills:` name check, and `skipped_capability_check` is
/// set the first time one of those checks has no `config` to check against,
/// so the report can note it once rather than repeat the same caveat next to
/// every name. `issues` and `visited` used to be separate `&mut` parameters
/// threaded through every function below (`lint_node`/`lint_workflow_node`/
/// `lint_sub_workflow`/`check_capability_name_lists`/`check_capability_names`
/// each took both); folding them in here removes that repetition the same
/// way `workflow::dryrun::DryRunContext` bundles its own call-spanning,
/// mostly-invariant state. `base_dir`/`json_schemas` stay as explicit
/// parameters instead, since — unlike `issues`/`visited` — they actually
/// change with every `workflow:` node recursed into (each sub-workflow file
/// has its own directory and its own `json_schemas:` block).
struct LintCtx<'a> {
    config: Option<&'a ConfigFile>,
    skipped_capability_check: bool,
    issues: Vec<LintIssue>,
    /// Canonical paths of every workflow file currently being linted, top to
    /// bottom of the current `workflow:` chain — mirrors
    /// `WorkflowScope::nested`'s cycle/depth-cap bookkeeping at `run` time
    /// (see `check_workflow_nesting`).
    visited: Vec<PathBuf>,
    /// Sub-workflow files already loaded during this `lint_file` call, keyed
    /// by canonical path: a `workflow:` file referenced by more than one
    /// sibling node (not a cycle — a cycle is rejected before this cache is
    /// consulted) would otherwise be re-read and re-parsed from disk once
    /// per reference. Mirrors `workflow::WorkflowRegistry`'s per-path cache
    /// at `run` time, which `lint` had no equivalent of until now. `Rc`
    /// rather than `Arc`: a `LintCtx` never leaves the single thread
    /// `lint_file` runs it on, even when `LintRun::collect` lints several
    /// files concurrently (each file gets its own `LintCtx`).
    ///
    /// This only avoids the duplicate disk read + YAML parse — every
    /// reference site still fully re-walks the (shared) parsed tree via
    /// `lint_workflow_contents`, since two reference sites can have
    /// different `visited` chains (affecting cycle detection) and each
    /// needs its own "in 'workflow: X'" message attribution.
    loaded_workflows: HashMap<PathBuf, Rc<workflow::WorkflowFile>>,
}

impl<'a> LintCtx<'a> {
    fn new(config: Option<&'a ConfigFile>) -> Self {
        Self {
            config,
            skipped_capability_check: false,
            issues: Vec::new(),
            visited: Vec::new(),
            loaded_workflows: HashMap::new(),
        }
    }
}

fn lint_agent_file(path: &Path, config: Option<&ConfigFile>) -> LintReport {
    let mut ctx = LintCtx::new(config);

    match agent::load_agent(path) {
        Err(error) => {
            let line = yaml_error_line(&error);
            ctx.issues
                .push(LintIssue::error(format!("{error:#}")).with_line(line));
        }
        Ok(agent_file) => lint_agent_contents("the agent", &agent_file, &mut ctx),
    }

    note_skipped_capability_check(&mut ctx);
    LintReport {
        file: path.to_path_buf(),
        issues: ctx.issues,
    }
}

fn note_skipped_capability_check(ctx: &mut LintCtx) {
    if ctx.skipped_capability_check {
        ctx.issues.push(LintIssue::warning(format!(
            "'mcp'/'skills'/'subagents'/'tools' names were not checked because no {} was found \
             (or --no-config was used)",
            config::CONFIG_FILE_NAME
        )));
    }
}

/// Checks the parts of an agent file that `agent::load_agent` doesn't
/// already validate: its system prompt template's handlebars syntax, its
/// `input_schema`/`output_schema` (when set as an inline schema or a file
/// path, whichever resolves without error), and its `mcp:`/`skills:` names.
/// `context` names where this agent file came from in a lint message (e.g.
/// `"the agent"` for a top-level `agent run`/`agent lint` target, or `"node
/// 'x''s agent"` for a workflow node's `agent:`).
fn lint_agent_contents(context: &str, agent_file: &AgentFile, ctx: &mut LintCtx) {
    check_prompt_template(
        context,
        "system prompt template",
        &agent_file.system_prompt_template,
        &mut ctx.issues,
    );

    check_schema_entry(
        context,
        "input_schema",
        agent_file.input_schema.as_ref(),
        &mut ctx.issues,
    );
    check_schema_entry(
        context,
        "output_schema",
        agent_file.output_schema.as_ref(),
        &mut ctx.issues,
    );
    // `structured_output: true` requires `output_schema` (checked at parse
    // time by `agent::parse_agent`), so this is reached only when a
    // `schema_name` (the agent's own, or the "structured_output" default) is
    // actually sent as the Structured Outputs request's schema name — see
    // the matching check in `lint_node`.
    if agent_file.structured_output
        && let Err(error) = schema::validate_schema_name(agent_file.schema_name())
    {
        ctx.issues.push(LintIssue::error(format!(
            "{context} has an invalid 'schema_name': {error:#}"
        )));
    }

    check_capability_name_lists(
        context,
        agent_file.mcp.as_deref(),
        agent_file.skills.as_deref(),
        agent_file.subagents.as_deref(),
        agent_file.tools.as_deref(),
        ctx,
    );
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
fn check_mcp_allowed_tools_not_empty(context: &str, names: Option<&[String]>, ctx: &mut LintCtx) {
    let Some(names) = names else { return };
    let Some(config) = ctx.config else { return };
    for name in names {
        if let Some(server) = config.mcp_servers.get(name)
            && let Some(allowed_tools) = &server.allowed_tools
            && allowed_tools.is_empty()
        {
            ctx.issues.push(LintIssue::warning(format!(
                "{context} references MCP server '{name}', whose 'allowed_tools' in {} is an empty list; every tool call to it will be rejected at runtime",
                config::CONFIG_FILE_NAME
            )));
        }
    }
}

/// Checks all four capability-name lists (`mcp`/`skills`/`subagents`/
/// `tools`) a node/agent file may declare, in one call — every call site
/// below always checks all four together. This used to be four near-
/// identical 12-line wrappers (`check_mcp_names`/`check_skill_names`/
/// `check_subagent_names`/`check_tool_names`) around `check_capability_names`,
/// differing only in three literals and a closure each, called as a group of
/// four from every site; folding them into one function removes both that
/// repetition and each call site's own repeated four-call group.
fn check_capability_name_lists(
    context: &str,
    mcp: Option<&[String]>,
    skills: Option<&[String]>,
    subagents: Option<&[String]>,
    tools: Option<&[String]>,
    ctx: &mut LintCtx,
) {
    check_capability_names(
        context,
        "MCP server",
        "mcp_servers:",
        mcp,
        |config, name| config.mcp_servers.contains_key(name),
        ctx,
    );
    check_mcp_allowed_tools_not_empty(context, mcp, ctx);
    check_capability_names(
        context,
        "skill",
        "skills:",
        skills,
        |config, name| config.skills.contains_key(name),
        ctx,
    );
    check_capability_names(
        context,
        "subagent",
        "agents:",
        subagents,
        |config, name| config.agents.contains_key(name),
        ctx,
    );
    check_capability_names(
        context,
        "tool",
        "tools:",
        tools,
        |config, name| config.tools.contains_key(name),
        ctx,
    );
}

/// Shared by `check_capability_name_lists`: looks up a list of
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
            ctx.issues.push(LintIssue::error(format!(
                "{context} references unknown {kind} '{name}'; define it under '{field}' in {}",
                config::CONFIG_FILE_NAME
            )));
        }
    }
}

#[cfg(test)]
mod tests;
