//! Workflow-YAML lint rules: everything `lint_workflow_file` walks to find
//! problems `workflow::load_workflow` doesn't already catch (an unused
//! node, a dangling reference, an invalid jq filter/template/schema). Named
//! `workflow_lint` (not `workflow`) to avoid colliding with `crate::workflow`
//! (the workflow engine itself), which this module imports and refers to via
//! its ordinary `workflow::` prefix throughout. `pub(super)` throughout —
//! every item here is reached from `lint.rs` (`lint_workflow_file` itself,
//! plus `check_prompt_template`/`check_schema_entry`/`yaml_error_line`,
//! which `lint.rs`'s own `lint_agent_file`/`lint_agent_contents` also call)
//! or, transitively, from `lint/tests.rs` via `lint.rs`'s
//! `use workflow_lint::*;` re-export — never from outside the crate.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::{
    agent,
    config::ConfigFile,
    jq,
    nesting::{MAX_WORKFLOW_DEPTH, NestingDepthError, check_workflow_nesting},
    schema, template, workflow,
};

use super::{
    LintCtx, LintIssue, LintReport, check_capability_name_lists, lint_agent_contents,
    note_skipped_capability_check,
};

/// The 1-based line a YAML parse failure happened at, when `error`'s chain
/// contains a `serde_yaml::Error` that reports one (see
/// `serde_yaml::Error::location`). Unlike `guess_line`'s message-text
/// heuristic (used for every other kind of issue), this is an exact
/// position straight from the parser.
pub(super) fn yaml_error_line(error: &anyhow::Error) -> Option<usize> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<serde_yaml::Error>())
        .and_then(|error| error.location())
        .map(|location| location.line())
}

pub(super) fn lint_workflow_file(path: &Path, config: Option<&ConfigFile>) -> LintReport {
    let mut ctx = LintCtx::new(config);

    match workflow::load_workflow(path) {
        Err(error) => {
            let line = yaml_error_line(&error);
            ctx.issues
                .push(LintIssue::error(format!("{error:#}")).with_line(line));
        }
        Ok(wf) => {
            // Seeded with this file's own canonical path so a `workflow:`
            // chain that loops back to it is caught the same way
            // `WorkflowScope::nested` catches it at `run` time.
            let canonical = match std::fs::canonicalize(path) {
                Ok(canonical) => Some(canonical),
                Err(error) => {
                    // Falling back to `path.parent()` below is not
                    // equivalent: if `path` itself contains an unresolved
                    // symlink component, its raw parent can differ from the
                    // canonical parent `run` would use, so lint could then
                    // inspect a different set of sub-workflow files than
                    // `run` actually would. Surface it instead of silently
                    // linting under a possibly-wrong base directory.
                    ctx.issues.push(LintIssue::warning(format!(
                        "failed to canonicalize '{}' ({error}); sub-workflow \
                         resolution falls back to its non-canonical parent \
                         directory, which may differ from what `lait run` uses",
                        path.display()
                    )));
                    None
                }
            };
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
                ctx.visited.push(canonical);
            }
            lint_workflow_contents(&wf, &base_dir, &mut ctx);
        }
    }

    note_skipped_capability_check(&mut ctx);
    LintReport {
        file: path.to_path_buf(),
        issues: ctx.issues,
    }
}

/// Walks one already-loaded workflow's `nodes:`/`steps:` looking for
/// problems `workflow::load_workflow` doesn't already catch: nodes that are
/// defined but never used, and references (`agent:`/`workflow:`/`mcp:`/
/// `skills:`/schema names/jq filters/templates) that would only fail lazily,
/// at `run` time. Recurses into every `workflow:` node's sub-workflow file
/// (resolved against `base_dir`, the directory `wf`'s own file lives in) and
/// every `agent:` node's agent file.
pub(super) fn lint_workflow_contents(
    wf: &workflow::WorkflowFile,
    base_dir: &Path,
    ctx: &mut LintCtx,
) {
    let mut used_node_ids = HashSet::new();
    walk_steps(&wf.steps, &mut used_node_ids, &mut ctx.issues);
    for node_id in wf.nodes.keys() {
        if !used_node_ids.contains(node_id.as_str()) {
            ctx.issues.push(LintIssue::warning(format!(
                "node '{node_id}' is defined in 'nodes:' but never referenced by a step's 'use'"
            )));
        }
    }

    check_capability_name_lists(
        "the workflow's 'default'",
        wf.default.mcp.as_deref(),
        wf.default.skills.as_deref(),
        wf.default.subagents.as_deref(),
        wf.default.tools.as_deref(),
        ctx,
    );
    if let Some(system_prompt) = &wf.default.system_prompt {
        check_prompt_template(
            "the workflow's 'default'",
            "'system_prompt' template",
            system_prompt,
            &mut ctx.issues,
        );
    }

    for (node_id, node) in &wf.nodes {
        lint_node(node_id, node, base_dir, &wf.json_schemas, ctx);
    }
}

/// Records every `use:` id reached from `steps` (recursing into `on_error`
/// and every router kind's nested `steps`/`cases`/`branches`) into `used`,
/// and checks every jq filter site (`when`, `switch` case `when`s, `loop`
/// `while`/`until`, `for_each` `items`, `parallel`/`for_each` `join`) for
/// syntax errors along the way. Mirrors the tree `workflow::validate_steps`
/// walks, so a router kind added there needs updating here too.
pub(super) fn walk_steps<'a>(
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

pub(super) fn check_jq(filter: &str, description: &str, issues: &mut Vec<LintIssue>) {
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
pub(super) fn lint_node(
    node_id: &str,
    node: &workflow::NodeDefinition,
    base_dir: &Path,
    json_schemas: &schema::JsonSchemaMap,
    ctx: &mut LintCtx,
) {
    let node_context = format!("node '{node_id}'");
    let settings = node.settings();

    if let Some(filter) = settings.jq {
        check_jq(filter, &format!("{node_context}: 'jq'"), &mut ctx.issues);
    }
    check_capability_name_lists(
        &node_context,
        settings.mcp,
        settings.skills,
        settings.subagents,
        settings.tools,
        ctx,
    );

    match node {
        workflow::NodeDefinition::Prompt(prompt) => {
            lint_prompt_node(
                node_id,
                &node_context,
                prompt,
                json_schemas,
                &mut ctx.issues,
            );
        }
        workflow::NodeDefinition::Agent(agent_node) => {
            lint_agent_node(node_id, agent_node, ctx);
        }
        workflow::NodeDefinition::Workflow(workflow_node) => {
            lint_workflow_node(node_id, workflow_node, base_dir, ctx);
        }
        workflow::NodeDefinition::Command(command) => {
            lint_command_node(&node_context, command, &mut ctx.issues);
        }
        workflow::NodeDefinition::Transform(_) => {}
        workflow::NodeDefinition::Ask(ask) => {
            check_prompt_template(
                &node_context,
                "'prompt' template",
                &ask.prompt,
                &mut ctx.issues,
            );
        }
    }
}

pub(super) fn lint_prompt_node(
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

pub(super) fn lint_agent_node(node_id: &str, agent_node: &workflow::AgentNode, ctx: &mut LintCtx) {
    // Matches `execute_step`: `agent:` is loaded as given, relative
    // to the current working directory (unlike `workflow:`, which
    // resolves against the workflow file's own directory) — see
    // `AgentNode::agent`'s doc comment.
    match agent::load_agent(&agent_node.agent) {
        Ok(agent_file) => {
            lint_agent_contents(&format!("node '{node_id}''s agent"), &agent_file, ctx);
        }
        Err(error) => ctx.issues.push(LintIssue::error(format!(
            "node '{node_id}' has 'agent: {}' (resolved relative to the current working \
             directory, not this workflow file), which failed to load: {error:#}",
            agent_node.agent.display()
        ))),
    }
}

pub(super) fn lint_workflow_node(
    node_id: &str,
    workflow_node: &workflow::WorkflowNode,
    base_dir: &Path,
    ctx: &mut LintCtx,
) {
    lint_sub_workflow(node_id, &workflow_node.workflow, base_dir, ctx);
}

pub(super) fn lint_command_node(
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
pub(super) fn check_prompt_template(
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
pub(super) fn check_schema_entry(
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
pub(super) fn check_unrecognized_schema_types(
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

pub(super) fn lint_sub_workflow(
    node_id: &str,
    sub_workflow_path: &Path,
    base_dir: &Path,
    ctx: &mut LintCtx,
) {
    let resolved = base_dir.join(sub_workflow_path);
    let canonical = match std::fs::canonicalize(&resolved) {
        Ok(canonical) => canonical,
        Err(error) => {
            ctx.issues.push(LintIssue::error(format!(
                "node '{node_id}' has 'workflow: {}', which could not be resolved: {error}",
                sub_workflow_path.display()
            )));
            return;
        }
    };
    // Shares `WorkflowScope::nested`'s cycle/depth-cap check, so a
    // non-cyclic-but-arbitrarily-deep or cyclic `workflow:` chain is flagged
    // here the same way it would fail at `run` time.
    if let Err(error) = check_workflow_nesting(&ctx.visited, &canonical) {
        ctx.issues.push(LintIssue::error(match error {
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

    // See `LintCtx::loaded_workflows`'s doc comment: this only skips the
    // disk read + YAML parse on a repeat reference, not the lint itself.
    let sub_wf = match ctx.loaded_workflows.get(&canonical) {
        Some(cached) => Rc::clone(cached),
        None => match workflow::load_workflow(&resolved) {
            Err(error) => {
                ctx.issues.push(LintIssue::error(format!(
                    "node '{node_id}' has 'workflow: {}', which failed to load: {error:#}",
                    sub_workflow_path.display()
                )));
                return;
            }
            Ok(loaded) => {
                let loaded = Rc::new(loaded);
                ctx.loaded_workflows
                    .insert(canonical.clone(), Rc::clone(&loaded));
                loaded
            }
        },
    };

    let sub_base_dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    ctx.visited.push(canonical);
    // `lint_workflow_contents` pushes straight into `ctx.issues`, so
    // without this, a message from `sub_workflow_path`'s own
    // 'nodes:'/'steps:' (e.g. an unused-node warning, whose node ids
    // are only unique within their own file) would print under the
    // top-level file's header with nothing saying which file it
    // actually came from. Prefix every message this recursive call
    // adds with the sub-workflow's path to attribute it.
    let issues_before = ctx.issues.len();
    lint_workflow_contents(&sub_wf, &sub_base_dir, ctx);
    for issue in &mut ctx.issues[issues_before..] {
        issue.message = format!(
            "in 'workflow: {}': {}",
            sub_workflow_path.display(),
            issue.message
        );
    }
    ctx.visited.pop();
}
