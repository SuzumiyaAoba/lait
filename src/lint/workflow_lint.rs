//! Workflow-YAML lint rules: everything `lint_workflow_file` walks to find
//! problems `workflow::load_workflow` doesn't already catch (it has checked
//! shape, syntax, ids, and placement): references that only fail lazily at
//! run time — `{{ inputs.* }}`/`$inputs.*` naming an undeclared input,
//! `{{ steps.* }}`/`$steps.*` naming an unknown step id, unloadable
//! schema/agent/workflow files, unknown `mcp`/`skills`/`subagents`/`tools`
//! names — plus definitions that are never used. Named `workflow_lint` (not
//! `workflow`) to avoid colliding with `crate::workflow` (the workflow engine
//! itself), which this module imports and refers to via its ordinary
//! `workflow::` prefix throughout. `pub(super)` throughout — every item here
//! is reached from `lint.rs` (`lint_workflow_file` itself, plus
//! `check_prompt_template`/`check_schema_source`/`yaml_error_line`, which
//! `lint.rs`'s own `lint_agent_file`/`lint_agent_contents` also call) or,
//! transitively, from `lint/tests.rs` via `lint.rs`'s `use workflow_lint::*;`
//! re-export — never from outside the crate.

use std::{collections::HashSet, path::Path, rc::Rc};

use crate::{
    agent,
    config::{self, ConfigFile},
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
            // `WorkflowScope::check_nested_path` catches it at `run` time.
            match std::fs::canonicalize(path) {
                Ok(canonical) => ctx.visited.push(canonical),
                Err(error) => ctx.issues.push(LintIssue::warning(format!(
                    "failed to canonicalize '{}' ({error}); a 'workflow:' cycle back to this \
                     file cannot be detected",
                    path.display()
                ))),
            }
            lint_workflow_contents(&wf, &mut ctx);
        }
    }

    note_skipped_capability_check(&mut ctx);
    LintReport {
        file: path.to_path_buf(),
        issues: ctx.issues,
    }
}

/// Walks one already-loaded workflow looking for problems
/// `workflow::load_workflow` doesn't already catch (see this module's doc
/// comment). Recurses into every `workflow:` step's child file and every
/// agent file.
pub(super) fn lint_workflow_contents(wf: &workflow::WorkflowFile, ctx: &mut LintCtx) {
    let defaults_context = "the workflow's 'default'";
    check_capability_name_lists(
        defaults_context,
        wf.defaults.mcp.as_deref(),
        wf.defaults.skills.as_deref(),
        wf.defaults.subagents.as_deref(),
        wf.defaults.tools.as_deref(),
        ctx,
    );

    for (name, definition) in &wf.inputs {
        check_unrecognized_schema_types(
            &format!("input '{name}'"),
            &definition.schema,
            &mut ctx.issues,
        );
    }
    for (name, source) in &wf.schemas {
        match schema::load_schema_value(source) {
            Ok(value) => check_unrecognized_schema_types(
                &format!("schema '{name}'"),
                &value,
                &mut ctx.issues,
            ),
            Err(error) => ctx.issues.push(LintIssue::error(format!(
                "schema '{name}' is invalid: {error:#}"
            ))),
        }
    }
    for (name, definition) in &wf.agents {
        lint_agent_contents(&format!("agent '{name}'"), definition, ctx);
    }

    let mut refs = References::default();
    collect_ids(&wf.steps, &mut refs.ids);
    refs.inputs = wf.inputs.iter().map(|(name, _)| name.clone()).collect();
    if let Some(output) = &wf.output {
        refs.check_jq("the workflow's 'output'", output, &mut ctx.issues);
    }
    if let Some(system) = &wf.defaults.system {
        refs.check_template(defaults_context, system, &mut ctx.issues);
    }
    for (name, definition) in &wf.agents {
        refs.check_template(
            &format!("agent '{name}'"),
            &definition.system_prompt_template,
            &mut ctx.issues,
        );
    }

    let mut used_schemas = HashSet::new();
    let mut used_agents = HashSet::new();
    if let Some(input_schema) = &wf.input_schema {
        match &input_schema.name {
            Some(name) => {
                used_schemas.insert(name.clone());
            }
            None => check_schema_source(
                "the workflow",
                "input_schema",
                &input_schema.source,
                &mut ctx.issues,
            ),
        }
    }
    lint_steps(
        &wf.steps,
        &mut StepLint {
            ctx,
            refs: &refs,
            used_schemas: &mut used_schemas,
            used_agents: &mut used_agents,
        },
    );
    for name in wf.schemas.keys() {
        if !used_schemas.contains(name) {
            ctx.issues.push(LintIssue::warning(format!(
                "schema '{name}' is defined in 'schemas:' but never referenced"
            )));
        }
    }
    for name in wf.agents.keys() {
        if !used_agents.contains(name) {
            ctx.issues.push(LintIssue::warning(format!(
                "agent '{name}' is defined in 'agents:' but never used by an 'agent:' step"
            )));
        }
    }
}

/// The names a workflow's templates and jq expressions may reference.
#[derive(Default)]
pub(super) struct References {
    ids: HashSet<String>,
    inputs: HashSet<String>,
}

impl References {
    fn check_template(&self, context: &str, source: &str, issues: &mut Vec<LintIssue>) {
        self.check_names(
            context,
            template::referenced_fields(source, "inputs"),
            template::referenced_fields(source, "steps"),
            issues,
        );
    }

    fn check_jq(&self, context: &str, filter: &str, issues: &mut Vec<LintIssue>) {
        self.check_names(
            context,
            jq_referenced_fields(filter, "$inputs"),
            jq_referenced_fields(filter, "$steps"),
            issues,
        );
    }

    fn check_names(
        &self,
        context: &str,
        inputs: Vec<String>,
        steps: Vec<String>,
        issues: &mut Vec<LintIssue>,
    ) {
        for name in inputs {
            if !self.inputs.contains(&name) {
                issues.push(LintIssue::error(format!(
                    "{context} references input '{name}', which is not declared in 'inputs:'"
                )));
            }
        }
        for id in steps {
            if !self.ids.contains(&id) {
                issues.push(LintIssue::warning(format!(
                    "{context} references step '{id}', but no step in this file has that 'id'"
                )));
            }
        }
    }
}

/// The first path segment after every `<root>.` in a jq filter (e.g.
/// `lang` in `$inputs.lang`, or `x` in `$steps["x"]`).
pub(super) fn jq_referenced_fields(filter: &str, root: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = filter;
    while let Some(start) = rest.find(root) {
        let after = &rest[start + root.len()..];
        let name: String = if let Some(tail) = after.strip_prefix('.') {
            tail.chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        } else if let Some(tail) = after.strip_prefix("[\"") {
            tail.chars().take_while(|c| *c != '"').collect()
        } else {
            String::new()
        };
        if !name.is_empty() && !found.contains(&name) {
            found.push(name);
        }
        rest = after;
    }
    found
}

fn collect_ids(steps: &[workflow::Step], ids: &mut HashSet<String>) {
    for step in steps {
        if let Some(id) = &step.id {
            ids.insert(id.clone());
        }
        step.for_each_child(|_, children| collect_ids(children, ids));
    }
}

/// The per-file state one `lint_steps` walk threads through every step, on
/// top of `LintCtx`'s per-`lint_file` state.
struct StepLint<'a, 'b> {
    ctx: &'a mut LintCtx<'b>,
    refs: &'a References,
    used_schemas: &'a mut HashSet<String>,
    used_agents: &'a mut HashSet<String>,
}

fn lint_steps(steps: &[workflow::Step], lint: &mut StepLint) {
    for (index, step) in steps.iter().enumerate() {
        lint_step(step, index + 1, lint);
    }
}

fn lint_step(step: &workflow::Step, position: usize, lint: &mut StepLint) {
    use workflow::StepKind;

    let context = match &step.id {
        Some(id) => format!("step '{id}'"),
        None => format!("step {position} ({})", step.kind.name()),
    };
    let refs = lint.refs;
    for (field, filter) in [
        ("when", &step.when),
        ("input", &step.input),
        ("output", &step.output),
    ] {
        if let Some(filter) = filter {
            refs.check_jq(
                &format!("{context}'s '{field}'"),
                filter,
                &mut lint.ctx.issues,
            );
        }
    }

    match &step.kind {
        StepKind::Prompt(prompt) => {
            refs.check_template(&context, &prompt.prompt, &mut lint.ctx.issues);
            if let Some(system) = &prompt.system {
                refs.check_template(&context, system, &mut lint.ctx.issues);
            }
            for source in prompt
                .attachments
                .files
                .iter()
                .chain(&prompt.attachments.images)
            {
                refs.check_template(&context, source, &mut lint.ctx.issues);
            }
            lint_llm(&context, &prompt.llm, lint.ctx);
            for (field, schema_ref) in [
                ("input_schema", &prompt.input_schema),
                ("output_schema", &prompt.output_schema),
            ] {
                let Some(schema_ref) = schema_ref else {
                    continue;
                };
                if let Some(name) = &schema_ref.name {
                    lint.used_schemas.insert(name.clone());
                } else {
                    check_schema_source(&context, field, &schema_ref.source, &mut lint.ctx.issues);
                }
            }
        }
        StepKind::Agent(agent_step) => {
            lint_llm(&context, &agent_step.llm, lint.ctx);
            for source in agent_step
                .attachments
                .files
                .iter()
                .chain(&agent_step.attachments.images)
            {
                refs.check_template(&context, source, &mut lint.ctx.issues);
            }
            match &agent_step.agent {
                workflow::AgentRef::Inline { name, .. } => {
                    lint.used_agents.insert(name.clone());
                }
                workflow::AgentRef::Path(path) => lint_agent_path(&context, path, lint.ctx),
                workflow::AgentRef::Registry(name) => match lint.ctx.config {
                    Some(config) => match config.agents.get(name) {
                        Some(path) => {
                            let path = path.clone();
                            lint_agent_path(&context, &path, lint.ctx);
                        }
                        None => lint.ctx.issues.push(LintIssue::error(format!(
                            "{context} uses agent '{name}', which is neither defined in this \
                             workflow's 'agents:' nor registered under 'agents:' in {} (use a \
                             path such as './{name}.md' for a file)",
                            config::CONFIG_FILE_NAME
                        ))),
                    },
                    None => lint.ctx.skipped_capability_check = true,
                },
            }
        }
        StepKind::Run(run) => {
            for arg in &run.argv {
                refs.check_template(&context, arg, &mut lint.ctx.issues);
            }
        }
        StepKind::Workflow(workflow_step) => {
            if let Some(with) = &workflow_step.with {
                refs.check_jq(&format!("{context}'s 'with'"), with, &mut lint.ctx.issues);
            }
            let path = match &workflow_step.workflow {
                workflow::WorkflowRef::Path(path) => Some(path.clone()),
                workflow::WorkflowRef::Registry(name) => match lint.ctx.config {
                    Some(config) => match config.workflows.get(name) {
                        Some(path) => Some(path.clone()),
                        None => {
                            lint.ctx.issues.push(LintIssue::error(format!(
                                "{context} runs workflow '{name}', which is not registered under \
                                 'workflows:' in {} (use a path such as './{name}.yml' for a file)",
                                config::CONFIG_FILE_NAME
                            )));
                            None
                        }
                    },
                    None => {
                        lint.ctx.skipped_capability_check = true;
                        None
                    }
                },
            };
            if let Some(path) = path {
                lint_sub_workflow(&context, &path, lint.ctx);
            }
        }
        StepKind::Jq(filter) => {
            refs.check_jq(&format!("{context}'s 'jq'"), filter, &mut lint.ctx.issues)
        }
        StepKind::Ask(ask) => refs.check_template(&context, &ask.prompt, &mut lint.ctx.issues),
        StepKind::Write(write) => refs.check_template(&context, &write.path, &mut lint.ctx.issues),
        StepKind::Switch(switch) => {
            for case in &switch.cases {
                refs.check_jq(
                    &format!("{context}'s case 'when'"),
                    &case.when,
                    &mut lint.ctx.issues,
                );
            }
        }
        StepKind::ForEach(for_each) => refs.check_jq(
            &format!("{context}'s 'for_each'"),
            &for_each.items,
            &mut lint.ctx.issues,
        ),
        StepKind::Loop(loop_step) => refs.check_jq(
            &format!("{context}'s '{}'", loop_step.condition.keyword()),
            loop_step.condition.filter(),
            &mut lint.ctx.issues,
        ),
        StepKind::Group(_) | StepKind::Parallel(_) | StepKind::Stop | StepKind::Break => {}
    }

    step.for_each_child(|_, children| lint_steps(children, lint));
}

fn lint_llm(context: &str, llm: &workflow::LlmOverrides, ctx: &mut LintCtx) {
    check_capability_name_lists(
        context,
        llm.mcp.as_deref(),
        llm.skills.as_deref(),
        llm.subagents.as_deref(),
        llm.tools.as_deref(),
        ctx,
    );
}

fn lint_agent_path(context: &str, path: &Path, ctx: &mut LintCtx) {
    match agent::load_agent(path) {
        Ok(agent_file) => lint_agent_contents(&format!("{context}'s agent"), &agent_file, ctx),
        Err(error) => ctx.issues.push(LintIssue::error(format!(
            "{context} uses agent '{}', which failed to load: {error:#}",
            path.display()
        ))),
    }
}

/// Checks a system-prompt template's handlebars syntax (an agent file's
/// Markdown body; workflow templates are already checked at load time).
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

/// Checks that a schema source loads (a `{file: ...}` exists and parses)
/// and warns about unrecognized `type` names in it.
pub(super) fn check_schema_source(
    context: &str,
    field: &str,
    source: &schema::SchemaSource,
    issues: &mut Vec<LintIssue>,
) {
    match schema::load_schema_value(source) {
        Ok(resolved) => {
            check_unrecognized_schema_types(&format!("{context}'s '{field}'"), &resolved, issues);
        }
        Err(error) => issues.push(LintIssue::error(format!(
            "{context}'s '{field}' is invalid: {error:#}"
        ))),
    }
}

/// Warns about every `type` keyword value in `schema` that isn't one of
/// JSON Schema's recognized primitive type names — lait's validator treats
/// an unrecognized name as matching any value, so a typo like `type: sting`
/// would otherwise leave that field completely unchecked.
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

pub(super) fn lint_sub_workflow(context: &str, path: &Path, ctx: &mut LintCtx) {
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) => {
            ctx.issues.push(LintIssue::error(format!(
                "{context} runs workflow '{}', which could not be resolved: {error}",
                path.display()
            )));
            return;
        }
    };
    // Shares `WorkflowScope::check_nested_path`'s cycle/depth-cap check, so
    // a non-cyclic-but-arbitrarily-deep or cyclic `workflow:` chain is
    // flagged here the same way it would fail at `run` time.
    if let Err(error) = check_workflow_nesting(&ctx.visited, &canonical) {
        ctx.issues.push(LintIssue::error(match error {
            NestingDepthError::Cycle => format!(
                "{context} runs workflow '{}', which would create a cycle ('{}' is already \
                 being linted)",
                path.display(),
                canonical.display()
            ),
            NestingDepthError::TooDeep => format!(
                "{context} runs workflow '{}', which exceeds the maximum 'workflow:' nesting \
                 depth of {MAX_WORKFLOW_DEPTH}",
                path.display()
            ),
        }));
        return;
    }

    // See `LintCtx::loaded_workflows`'s doc comment: this only skips the
    // disk read + YAML parse on a repeat reference, not the lint itself.
    let child = match ctx.loaded_workflows.get(&canonical) {
        Some(cached) => Rc::clone(cached),
        None => match workflow::load_workflow(&canonical) {
            Err(error) => {
                ctx.issues.push(LintIssue::error(format!(
                    "{context} runs workflow '{}', which failed to load: {error:#}",
                    path.display()
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

    ctx.visited.push(canonical);
    // Attribute every message from the child file to it, since its ids and
    // names are only meaningful within that file.
    let issues_before = ctx.issues.len();
    lint_workflow_contents(&child, ctx);
    for issue in &mut ctx.issues[issues_before..] {
        issue.message = format!("in workflow '{}': {}", path.display(), issue.message);
    }
    ctx.visited.pop();
}
