use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    cli::ReasoningEffort,
    config::{DefaultSettings, ModelMap},
    jq,
    schema::JsonSchemaMap,
    template,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowFile {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) default: DefaultSettings,
    /// Model aliases usable by `default.model`/`steps[].model`, in the same shape as
    /// `lait.config.yml`'s top-level `models:`. Takes precedence over an alias of
    /// the same name defined in `lait.config.yml`.
    #[serde(default)]
    pub(crate) models: ModelMap,
    /// Named schema definitions usable by `steps[].output_schema` and
    /// `steps[].input_schema`, each either a `file_path:` to a JSON schema
    /// file or an inline `schema:` body.
    #[serde(default)]
    pub(crate) json_schemas: JsonSchemaMap,
    pub(crate) steps: Vec<StepDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepDefinition {
    pub(crate) id: Option<String>,
    /// A jq filter evaluated against the current input (JSON-parsed, falling
    /// back to a JSON string for plain text, like `template::parse_input`).
    /// A falsy result (`false`/`null`) skips this step entirely, passing the
    /// input through unchanged to the next step. Mutually exclusive with
    /// `switch`.
    pub(crate) when: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// The prompt template sent to the model. A step without a `prompt` and
    /// without an `agent` does not call the model at all; it must then have a
    /// `jq` filter, making it a data-only transformation step. Mutually
    /// exclusive with `agent`.
    pub(crate) prompt: Option<String>,
    /// Path to an agent Markdown file (see `agent::load_agent`) whose system
    /// prompt, model/reasoning defaults, and input/output schema drive this
    /// step instead of `prompt`/`input_schema`/`output_schema`/`schema_name`.
    /// Mutually exclusive with `prompt`, `input_schema`, `output_schema`, and
    /// `schema_name`.
    pub(crate) agent: Option<PathBuf>,
    /// Validates this step's input before it runs (before rendering `prompt`,
    /// or before `jq` for a transform-only step). Resolved against the
    /// workflow's top-level `json_schemas:` first; if no such key exists,
    /// treated as a path to a JSON schema file instead. Mutually exclusive
    /// with `agent`, whose agent file supplies its own `input_schema`.
    pub(crate) input_schema: Option<String>,
    /// Request a structured JSON response using the named schema, like the CLI's
    /// `--json-schema`. Resolved against the workflow's top-level `json_schemas:`
    /// first; if no such key exists, treated as a path to a JSON schema file
    /// instead. Requires `prompt`.
    pub(crate) output_schema: Option<String>,
    /// The name of the structured output schema. Defaults to `structured_output`,
    /// like the CLI's `--schema-name`. Only used together with `output_schema`.
    pub(crate) schema_name: Option<String>,
    /// A jq filter applied to this step's output (the model's response, or the
    /// running input if there is no `prompt`) before it becomes `{{ input }}` for
    /// the next step. The filtered value must be valid JSON.
    pub(crate) jq: Option<String>,
    /// Retries this step's action (`input_schema` check, `prompt`/`agent`
    /// call, and `jq`, as one unit) up to `max_attempts` times on failure.
    /// Applies before `on_error`, which only runs once every attempt here
    /// has failed.
    pub(crate) retry: Option<RetryDefinition>,
    /// A per-attempt time limit, in seconds, on this step's action. A timed
    /// out attempt counts as a failure for `retry`, the same as any other
    /// error.
    pub(crate) timeout: Option<u64>,
    /// Runs in place of failing the workflow when this step's action (after
    /// every `retry` attempt, if any) still fails. Its steps receive
    /// `{"error": "<the failure message>", "input": <this step's input>}` as
    /// their `{{ input }}`; shape that with a leading `jq` step if the
    /// workflow needs something else.
    pub(crate) on_error: Option<OnErrorDefinition>,
    /// Turns this step into a branch router: evaluates `cases` in order and
    /// runs the first one whose `when` is truthy (or `else`, if none match).
    /// Mutually exclusive with every other field except `id` (including
    /// `parallel`).
    pub(crate) switch: Option<SwitchDefinition>,
    /// Turns this step into a fan-out/fan-in: runs every branch concurrently
    /// against the same input and joins their outputs. Mutually exclusive
    /// with every other field except `id` (including `switch`).
    pub(crate) parallel: Option<ParallelDefinition>,
    /// Turns this step into a conditional loop: re-runs `steps` while/until a
    /// jq condition holds, threading each iteration's output into the next
    /// iteration's `{{ input }}`. Mutually exclusive with every other field
    /// except `id` (including `switch`/`parallel`/`for_each`).
    pub(crate) r#loop: Option<LoopDefinition>,
    /// Turns this step into an array map: runs `steps` once per element of a
    /// jq-selected array, collecting the results (in array order) into a
    /// JSON array. Mutually exclusive with every other field except `id`
    /// (including `switch`/`parallel`/`loop`).
    pub(crate) for_each: Option<ForEachDefinition>,
    /// Ends the workflow successfully right after this step runs (after its
    /// own `prompt`/`agent`/`jq` action, if any), using this step's output as
    /// the workflow's final result; no further steps run. Rejected inside a
    /// `parallel` branch, where concurrently running sibling branches make
    /// "stop the workflow" ambiguous. Mutually exclusive with `break`.
    pub(crate) stop: Option<bool>,
    /// Exits the nearest enclosing `loop`/`for_each` body right after this
    /// step runs, using this step's output as that iteration's result (the
    /// loop then proceeds as if the iteration had finished normally, i.e.
    /// checking `while`/`until` or moving to `join`). Requires an enclosing
    /// `loop`/`for_each` reachable without crossing a `parallel` branch
    /// boundary. Mutually exclusive with `stop`.
    pub(crate) r#break: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetryDefinition {
    /// The total number of attempts, including the first (i.e. `3` means "try
    /// once, then retry up to twice more"). Required, and must be at least 1.
    pub(crate) max_attempts: Option<usize>,
    /// How long to wait, in seconds, before the first retry (after attempt 1
    /// fails). Defaults to 0 (retry immediately).
    pub(crate) delay_seconds: Option<u64>,
    /// Multiplies the wait before each subsequent retry (e.g. `2.0` doubles
    /// it every time). Defaults to `1.0` (a constant delay).
    pub(crate) backoff: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OnErrorDefinition {
    /// Run once, with the failure's `{"error": ..., "input": ...}` object as
    /// `{{ input }}`, in place of failing the workflow. `stop`/`break` are
    /// allowed here like anywhere else (subject to the same nesting rules as
    /// the failing step itself).
    pub(crate) steps: Vec<StepDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SwitchDefinition {
    /// Evaluated in order; the first case whose `when` is truthy runs.
    pub(crate) cases: Vec<CaseDefinition>,
    /// Runs when no `case` matched. Required unless the workflow author is
    /// sure `cases` is exhaustive: a `switch` with no matching case and no
    /// `else` is a runtime error rather than a silent pass-through.
    #[serde(rename = "else")]
    pub(crate) else_steps: Option<Vec<StepDefinition>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaseDefinition {
    /// An optional label used only in progress output (like `StepDefinition::id`).
    pub(crate) id: Option<String>,
    /// A jq filter evaluated against the current input; see `StepDefinition::when`.
    pub(crate) when: String,
    pub(crate) steps: Vec<StepDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParallelDefinition {
    /// Every branch runs concurrently against the same input (a snapshot of
    /// `{{ input }}` as it stood when the `parallel` step started). Their
    /// outputs are collected, in `branches` declaration order (not
    /// completion order, so the join is deterministic), into a JSON object
    /// keyed by each branch's `id` (or its default label; see
    /// `BranchDefinition::label`).
    pub(crate) branches: Vec<BranchDefinition>,
    /// A jq filter applied to that id-keyed object, the same way
    /// `StepDefinition::jq` applies to a single step's output. If omitted,
    /// the object itself (serialized as JSON) becomes `{{ input }}` for the
    /// next step.
    pub(crate) join: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchDefinition {
    /// Defaults to `branch-{n}` (1-based), like `StepDefinition::id`. Unlike
    /// a step or case id, this also becomes the branch's key in the joined
    /// JSON object, so it must be unique within its `parallel`.
    pub(crate) id: Option<String>,
    pub(crate) steps: Vec<StepDefinition>,
}

impl BranchDefinition {
    /// The label used both for progress output and as the branch's key in
    /// the joined JSON object. `index` is 0-based.
    pub(crate) fn label(&self, index: usize) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("branch-{}", index + 1))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoopDefinition {
    /// Checked before each iteration (including the first), against the
    /// current input; the loop runs while this is truthy, so it may run zero
    /// times. Mutually exclusive with `until`; exactly one of them is
    /// required.
    pub(crate) r#while: Option<String>,
    /// Checked after each iteration, against that iteration's output; the
    /// loop stops once this becomes truthy, so `steps` always runs at least
    /// once. Mutually exclusive with `while`; exactly one of them is
    /// required. Note the condition runs through the same JSON-or-string
    /// coercion as `when` (see `eval_when`), so the last step in `steps`
    /// must produce a value it can be evaluated against (via
    /// `output_schema` or `jq`) for anything beyond a plain truthy/falsy
    /// text check.
    pub(crate) until: Option<String>,
    /// Safety cap on the number of iterations. Required (and must be at
    /// least 1): reaching it without `while`/`until` being satisfied is a
    /// runtime error rather than a silent stop, so this is an assertion
    /// ("must finish within N iterations"), not just a safety valve.
    pub(crate) max_iterations: Option<usize>,
    /// The loop body, re-run each iteration. Each iteration's final output
    /// becomes `{{ input }}` for the next iteration (or, for the first
    /// iteration, this is the `loop` step's own incoming input).
    pub(crate) steps: Vec<StepDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForEachDefinition {
    /// A jq filter evaluated once against the current input; must produce
    /// exactly one output value, which must be a JSON array (e.g. `.items`,
    /// not a stream-producing filter like `.items[]`). Each element becomes
    /// one iteration's `{{ input }}`; unlike a `parallel` branch, the body
    /// cannot see anything of the surrounding input beyond that element.
    pub(crate) items: String,
    /// The loop body, run once per element of `items`, in array order.
    pub(crate) steps: Vec<StepDefinition>,
    /// A jq filter applied to the JSON array of per-element outputs (in
    /// `items` order), the same way `ParallelDefinition::join` applies to
    /// the id-keyed object from a `parallel` step. If omitted, the array
    /// itself (serialized as JSON) becomes `{{ input }}` for the next step.
    pub(crate) join: Option<String>,
}

pub(crate) fn load_workflow(path: &Path) -> Result<WorkflowFile> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read workflow file '{}'", path.display()))?;
    parse_workflow(&contents)
        .with_context(|| format!("failed to parse workflow file '{}'", path.display()))
}

fn parse_workflow(contents: &str) -> Result<WorkflowFile> {
    let workflow: WorkflowFile = serde_yaml::from_str(contents)?;
    if workflow.steps.is_empty() {
        bail!("workflow must contain at least one step");
    }
    validate_steps(&workflow.steps, FlowContext::TOP_LEVEL)?;
    Ok(workflow)
}

/// A named predicate over a `StepDefinition`, used by `ACTION_FIELDS`.
type ActionField = (&'static str, fn(&StepDefinition) -> bool);

/// The fields that drive a model call or data transform (as opposed to just
/// `id`), each paired with its name for use in an error message. Kept as a
/// single list so `has_action_fields` and `action_fields_desc` can't drift
/// out of sync when a field is added or removed.
const ACTION_FIELDS: &[ActionField] = &[
    ("when", |step| step.when.is_some()),
    ("model", |step| step.model.is_some()),
    ("reasoning_effort", |step| step.reasoning_effort.is_some()),
    ("prompt", |step| step.prompt.is_some()),
    ("agent", |step| step.agent.is_some()),
    ("input_schema", |step| step.input_schema.is_some()),
    ("output_schema", |step| step.output_schema.is_some()),
    ("schema_name", |step| step.schema_name.is_some()),
    ("jq", |step| step.jq.is_some()),
    ("retry", |step| step.retry.is_some()),
    ("timeout", |step| step.timeout.is_some()),
    ("on_error", |step| step.on_error.is_some()),
    ("stop", |step| step.stop.is_some()),
    ("break", |step| step.r#break.is_some()),
];

/// Whether `step` has any field that drives a model call or data transform
/// (as opposed to just `id`), used to reject a `switch`/`parallel`/`loop`/
/// `for_each` step that also sets one of these — they route to nested steps
/// instead of acting directly.
fn has_action_fields(step: &StepDefinition) -> bool {
    ACTION_FIELDS.iter().any(|(_, is_set)| is_set(step))
}

/// A human-readable, comma-separated list of `ACTION_FIELDS`' names, quoted
/// and with a trailing "or", for use in the "it cannot also have ..." bails.
fn action_fields_desc() -> String {
    let (last, rest) = ACTION_FIELDS
        .split_last()
        .expect("ACTION_FIELDS must not be empty");
    let quoted_rest: Vec<String> = rest.iter().map(|(name, _)| format!("'{name}'")).collect();
    format!("{}, or '{}'", quoted_rest.join(", "), last.0)
}

/// Rejects `step` if it has any `ACTION_FIELDS` set, since it is about to be
/// validated as a `router_name` (`switch`/`parallel`/`loop`/`for_each`) step,
/// which routes to nested steps instead of acting directly.
fn reject_action_fields_on_router(
    step: &StepDefinition,
    router_name: &str,
    label: &str,
) -> Result<()> {
    if has_action_fields(step) {
        bail!(
            "step '{label}' has '{router_name}' set; it cannot also have {}",
            action_fields_desc()
        );
    }
    Ok(())
}

/// Tracks the lexical nesting `validate_steps` is currently inside, used to
/// validate `break`/`stop` placement. `in_loop` requires an enclosing
/// `loop`/`for_each` body reachable without crossing a `parallel` branch
/// boundary (concurrently running branches can't share a single loop's break
/// target, so entering a branch resets it). `in_parallel_branch` marks any
/// depth inside a `parallel` branch, since there is no well-defined "the
/// workflow" to `stop` while sibling branches may still be running.
#[derive(Clone, Copy)]
struct FlowContext {
    in_loop: bool,
    in_parallel_branch: bool,
}

impl FlowContext {
    const TOP_LEVEL: Self = Self {
        in_loop: false,
        in_parallel_branch: false,
    };

    fn in_loop_body(self) -> Self {
        Self {
            in_loop: true,
            ..self
        }
    }

    fn in_parallel_branch(self) -> Self {
        Self {
            in_loop: false,
            in_parallel_branch: true,
        }
    }
}

fn validate_steps(steps: &[StepDefinition], ctx: FlowContext) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let label = || {
            step.id
                .clone()
                .unwrap_or_else(|| format!("step-{}", index + 1))
        };

        let router_count = [
            step.switch.is_some(),
            step.parallel.is_some(),
            step.r#loop.is_some(),
            step.for_each.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count();
        if router_count > 1 {
            bail!(
                "step '{}' can have at most one of 'switch', 'parallel', 'loop', or 'for_each'",
                label()
            );
        }

        if let Some(switch) = &step.switch {
            reject_action_fields_on_router(step, "switch", &label())?;
            if switch.cases.is_empty() {
                bail!("step '{}' has 'switch' with an empty 'cases' list", label());
            }
            for case in &switch.cases {
                if case.steps.is_empty() {
                    bail!(
                        "step '{}' has a 'switch' case with an empty 'steps' list",
                        label()
                    );
                }
                validate_steps(&case.steps, ctx)?;
            }
            if let Some(else_steps) = &switch.else_steps {
                if else_steps.is_empty() {
                    bail!(
                        "step '{}' has a 'switch' with an empty 'else' list",
                        label()
                    );
                }
                validate_steps(else_steps, ctx)?;
            }
            continue;
        }

        if let Some(parallel) = &step.parallel {
            reject_action_fields_on_router(step, "parallel", &label())?;
            if parallel.branches.is_empty() {
                bail!(
                    "step '{}' has 'parallel' with an empty 'branches' list",
                    label()
                );
            }
            let mut seen_branch_ids = std::collections::HashSet::new();
            for (branch_index, branch) in parallel.branches.iter().enumerate() {
                if branch.steps.is_empty() {
                    bail!(
                        "step '{}' has a 'parallel' branch with an empty 'steps' list",
                        label()
                    );
                }
                let branch_label = branch.label(branch_index);
                if !seen_branch_ids.insert(branch_label.clone()) {
                    bail!(
                        "step '{}' has 'parallel' branches with a duplicate id '{}'",
                        label(),
                        branch_label
                    );
                }
                validate_steps(&branch.steps, ctx.in_parallel_branch())?;
            }
            continue;
        }

        if let Some(loop_def) = &step.r#loop {
            reject_action_fields_on_router(step, "loop", &label())?;
            match (&loop_def.r#while, &loop_def.until) {
                (Some(_), Some(_)) => bail!(
                    "step '{}' has 'loop' with both 'while' and 'until'; exactly one is required",
                    label()
                ),
                (None, None) => bail!(
                    "step '{}' has 'loop' with neither 'while' nor 'until'; exactly one is required",
                    label()
                ),
                _ => {}
            }
            match loop_def.max_iterations {
                None => bail!(
                    "step '{}' has 'loop' with no 'max_iterations' (required)",
                    label()
                ),
                Some(0) => bail!(
                    "step '{}' has 'loop' with 'max_iterations: 0'; it must be at least 1",
                    label()
                ),
                Some(_) => {}
            }
            if loop_def.steps.is_empty() {
                bail!("step '{}' has 'loop' with an empty 'steps' list", label());
            }
            validate_steps(&loop_def.steps, ctx.in_loop_body())?;
            continue;
        }

        if let Some(for_each) = &step.for_each {
            reject_action_fields_on_router(step, "for_each", &label())?;
            if for_each.steps.is_empty() {
                bail!(
                    "step '{}' has 'for_each' with an empty 'steps' list",
                    label()
                );
            }
            validate_steps(&for_each.steps, ctx.in_loop_body())?;
            continue;
        }

        if let Some(retry) = &step.retry {
            match retry.max_attempts {
                None => bail!(
                    "step '{}' has 'retry' with no 'max_attempts' (required)",
                    label()
                ),
                Some(0) => bail!(
                    "step '{}' has 'retry' with 'max_attempts: 0'; it must be at least 1",
                    label()
                ),
                Some(_) => {}
            }
        }
        if step.timeout == Some(0) {
            bail!(
                "step '{}' has 'timeout: 0'; it must be at least 1 second",
                label()
            );
        }
        if let Some(on_error) = &step.on_error {
            if on_error.steps.is_empty() {
                bail!(
                    "step '{}' has 'on_error' with an empty 'steps' list",
                    label()
                );
            }
            validate_steps(&on_error.steps, ctx)?;
        }

        if step.r#break == Some(true) && step.stop == Some(true) {
            bail!(
                "step '{}' cannot have both 'stop: true' and 'break: true'",
                label()
            );
        }
        if step.r#break == Some(true) && !ctx.in_loop {
            bail!(
                "step '{}' has 'break: true' outside a 'loop'/'for_each' body",
                label()
            );
        }
        if step.stop == Some(true) && ctx.in_parallel_branch {
            bail!(
                "step '{}' has 'stop: true' inside a 'parallel' branch, where there is no \
                 single well-defined workflow to stop",
                label()
            );
        }

        let calls_model = step.prompt.is_some() || step.agent.is_some();
        if !calls_model && step.jq.is_none() && step.stop.is_none() && step.r#break.is_none() {
            bail!(
                "step '{}' must have a 'prompt', an 'agent', a 'jq' filter, a 'switch', a \
                 'parallel', a 'loop', a 'for_each', 'stop', 'break', or a combination",
                label()
            );
        }
        if step.prompt.is_some() && step.agent.is_some() {
            bail!("step '{}' cannot have both 'prompt' and 'agent'", label());
        }
        if step.agent.is_some()
            && (step.input_schema.is_some()
                || step.output_schema.is_some()
                || step.schema_name.is_some())
        {
            bail!(
                "step '{}' has 'agent' set; 'input_schema'/'output_schema'/'schema_name' come from the agent file and must not be set on the step",
                label()
            );
        }
        if !calls_model && step.output_schema.is_some() {
            bail!(
                "step '{}' has 'output_schema' but no 'prompt'/'agent' to apply it to",
                label()
            );
        }
        if step.output_schema.is_none() && step.schema_name.is_some() {
            bail!(
                "step '{}' has 'schema_name' but no 'output_schema'",
                label()
            );
        }
    }
    Ok(())
}

/// Replaces `{{ input }}` placeholders in a step's prompt template with `input`.
pub(crate) fn render_prompt(template: &str, input: &str) -> Result<String> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(relative_end) = rest[start..].find("}}") else {
            bail!("unterminated placeholder in prompt template: {template:?}");
        };
        let end = start + relative_end;
        rendered.push_str(&rest[..start]);
        let name = rest[start + 2..end].trim();
        match name {
            "input" => rendered.push_str(input),
            _ => bail!("unknown placeholder '{{{{ {name} }}}}' in prompt template"),
        }
        rest = &rest[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

/// Evaluates a `when`/case-condition jq filter against the current input,
/// using the same JSON-or-string coercion as `{{ input }}` templates
/// (`template::parse_input`) so a `when:` right after a plain-text `prompt:`
/// step doesn't fail just because the input isn't JSON.
pub(crate) fn eval_when(filter: &str, current_input: &str) -> Result<bool> {
    let value = template::parse_input(current_input);
    let input_json = serde_json::to_string(&value)
        .context("failed to serialize the current input for a 'when' condition")?;
    jq::apply_bool(filter, &input_json).context("failed to evaluate 'when' condition")
}

#[cfg(test)]
mod tests {
    use super::{eval_when, parse_workflow, render_prompt};
    use crate::schema::JsonSchemaEntry;

    #[test]
    fn renders_input_placeholder_with_and_without_spaces() {
        assert_eq!(
            render_prompt("summarize: {{input}}", "hello").unwrap(),
            "summarize: hello"
        );
        assert_eq!(
            render_prompt("summarize: {{ input }}", "hello").unwrap(),
            "summarize: hello"
        );
        assert_eq!(
            render_prompt("{{ input }} and {{ input }}", "x").unwrap(),
            "x and x"
        );
        assert_eq!(
            render_prompt("no placeholder here", "x").unwrap(),
            "no placeholder here"
        );
    }

    #[test]
    fn rejects_unknown_placeholder() {
        assert!(render_prompt("{{ nope }}", "x").is_err());
    }

    #[test]
    fn rejects_unterminated_placeholder() {
        assert!(render_prompt("{{ input", "x").is_err());
    }

    #[test]
    fn parses_workflow_with_multiple_steps() {
        let workflow = parse_workflow(
            r#"
name: example
description: summarize then translate
default:
  model: local
steps:
  - id: summarize
    prompt: "summarize: {{ input }}"
  - id: translate
    model: cloud
    reasoning_effort: high
    prompt: "translate: {{ input }}"
"#,
        )
        .expect("workflow should parse");

        assert_eq!(workflow.name.as_deref(), Some("example"));
        assert_eq!(workflow.default.model.as_deref(), Some("local"));
        assert_eq!(workflow.steps.len(), 2);
        assert_eq!(workflow.steps[0].id.as_deref(), Some("summarize"));
        assert_eq!(workflow.steps[1].model.as_deref(), Some("cloud"));
    }

    #[test]
    fn parses_workflow_with_embedded_models() {
        let workflow = parse_workflow(
            r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
      model_id: local-model
      default_reasoning_effort: medium
  cloud:
    - provider:
        base_url: https://api.example.com/v1
        api_key: secret
      model_id: cloud-model
steps:
  - prompt: "{{ input }}"
  - model: cloud
    prompt: "{{ input }}"
"#,
        )
        .expect("workflow with embedded models should parse");

        assert_eq!(workflow.models.len(), 2);
        assert!(workflow.models.contains_key("local"));
        assert!(workflow.models.contains_key("cloud"));
    }

    #[test]
    fn rejects_workflow_with_no_steps() {
        assert!(parse_workflow("steps: []\n").is_err());
    }

    #[test]
    fn parses_a_step_with_output_schema_and_jq() {
        let workflow = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    output_schema: schema.json
    schema_name: answer
    jq: ".answer"
"#,
        )
        .expect("workflow with output_schema and jq should parse");

        let step = &workflow.steps[0];
        assert_eq!(step.output_schema.as_deref(), Some("schema.json"));
        assert_eq!(step.schema_name.as_deref(), Some("answer"));
        assert_eq!(step.jq.as_deref(), Some(".answer"));
    }

    #[test]
    fn parses_a_workflow_with_inline_json_schemas() {
        let workflow = parse_workflow(
            r#"
json_schemas:
  answer:
    schema:
      type: object
      properties:
        answer:
          type: string
      required: [answer]
steps:
  - prompt: "{{ input }}"
    output_schema: answer
"#,
        )
        .expect("workflow with inline json_schemas should parse");

        assert_eq!(workflow.json_schemas.len(), 1);
        match &workflow.json_schemas["answer"] {
            JsonSchemaEntry::Inline { schema } => {
                assert_eq!(schema["properties"]["answer"]["type"], "string");
            }
            JsonSchemaEntry::FilePath { .. } => panic!("expected an inline schema entry"),
        }
        assert_eq!(workflow.steps[0].output_schema.as_deref(), Some("answer"));
    }

    #[test]
    fn parses_a_workflow_with_file_path_json_schemas() {
        let workflow = parse_workflow(
            r#"
json_schemas:
  answer:
    file_path: schema.json
steps:
  - prompt: "{{ input }}"
    output_schema: answer
"#,
        )
        .expect("workflow with file_path json_schemas should parse");

        match &workflow.json_schemas["answer"] {
            JsonSchemaEntry::FilePath { file_path } => {
                assert_eq!(file_path.to_str(), Some("schema.json"));
            }
            JsonSchemaEntry::Inline { .. } => panic!("expected a file_path schema entry"),
        }
    }

    #[test]
    fn rejects_a_json_schemas_entry_with_both_schema_and_file_path() {
        let result = parse_workflow(
            r#"
json_schemas:
  answer:
    schema:
      type: object
    file_path: schema.json
steps:
  - prompt: "{{ input }}"
    output_schema: answer
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_json_schemas_entry_with_neither_schema_nor_file_path() {
        let result = parse_workflow(
            r#"
json_schemas:
  answer: {}
steps:
  - prompt: "{{ input }}"
    output_schema: answer
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn allows_a_transform_only_step_with_no_prompt() {
        let workflow = parse_workflow(
            r#"
steps:
  - jq: ".answer"
"#,
        )
        .expect("a jq-only step should parse");

        assert!(workflow.steps[0].prompt.is_none());
        assert_eq!(workflow.steps[0].jq.as_deref(), Some(".answer"));
    }

    #[test]
    fn rejects_a_step_with_neither_prompt_nor_jq() {
        assert!(parse_workflow("steps:\n  - id: empty\n").is_err());
    }

    #[test]
    fn rejects_output_schema_without_a_prompt() {
        let result = parse_workflow("steps:\n  - jq: \".\"\n    output_schema: schema.json\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_schema_name_without_output_schema() {
        let result =
            parse_workflow("steps:\n  - prompt: \"{{ input }}\"\n    schema_name: answer\n");
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_step_with_an_agent() {
        let workflow = parse_workflow(
            r#"
steps:
  - agent: agents/extract.md
    jq: ".city"
"#,
        )
        .expect("workflow with an agent step should parse");

        assert_eq!(
            workflow.steps[0].agent.as_deref().and_then(|p| p.to_str()),
            Some("agents/extract.md")
        );
    }

    #[test]
    fn rejects_a_step_with_both_prompt_and_agent() {
        let result =
            parse_workflow("steps:\n  - prompt: \"{{ input }}\"\n    agent: agents/extract.md\n");
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_step_with_an_input_schema() {
        let workflow = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    input_schema: schema.json
"#,
        )
        .expect("workflow with an input_schema should parse");

        assert_eq!(
            workflow.steps[0].input_schema.as_deref(),
            Some("schema.json")
        );
    }

    #[test]
    fn rejects_a_step_with_agent_and_input_schema() {
        let result =
            parse_workflow("steps:\n  - agent: agents/extract.md\n    input_schema: schema.json\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_step_with_agent_and_output_schema() {
        let result = parse_workflow(
            "steps:\n  - agent: agents/extract.md\n    output_schema: schema.json\n",
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_step_with_agent_and_schema_name() {
        let result =
            parse_workflow("steps:\n  - agent: agents/extract.md\n    schema_name: answer\n");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let result = parse_workflow(
            r#"
unexpected: true
steps:
  - prompt: "{{ input }}"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_step_with_a_when_guard() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: maybe
    when: '. != null'
    prompt: "{{ input }}"
"#,
        )
        .expect("workflow with a 'when' guard should parse");

        assert_eq!(workflow.steps[0].when.as_deref(), Some(". != null"));
    }

    #[test]
    fn parses_a_switch_with_cases_and_else() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: route
    switch:
      cases:
        - id: high
          when: '.severity == "high"'
          steps:
            - prompt: "escalate: {{ input }}"
        - when: '.severity == "medium"'
          steps:
            - prompt: "reply: {{ input }}"
      else:
        - jq: ".summary"
"#,
        )
        .expect("workflow with a switch should parse");

        let switch = workflow.steps[0]
            .switch
            .as_ref()
            .expect("step should have a switch");
        assert_eq!(switch.cases.len(), 2);
        assert_eq!(switch.cases[0].id.as_deref(), Some("high"));
        assert!(switch.else_steps.is_some());
    }

    #[test]
    fn parses_a_switch_without_else() {
        let workflow = parse_workflow(
            r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
"#,
        )
        .expect("workflow with a switch without else should parse");

        assert!(
            workflow.steps[0]
                .switch
                .as_ref()
                .unwrap()
                .else_steps
                .is_none()
        );
    }

    #[test]
    fn rejects_a_switch_with_empty_cases() {
        let result = parse_workflow(
            r#"
steps:
  - switch:
      cases: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_switch_case_with_empty_steps() {
        let result = parse_workflow(
            r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_switch_with_an_empty_else() {
        let result = parse_workflow(
            r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
      else: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_switch_combined_with_prompt() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_switch_combined_with_input_schema() {
        let result = parse_workflow(
            r#"
steps:
  - input_schema: schema.json
    switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_switch_combined_with_when() {
        let result = parse_workflow(
            r#"
steps:
  - when: 'true'
    switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validates_steps_nested_inside_a_switch_case() {
        let result = parse_workflow(
            r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
              agent: agents/extract.md
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_parallel_with_branches_and_join() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: fan-out
    parallel:
      branches:
        - id: a
          steps:
            - prompt: "a: {{ input }}"
        - id: b
          steps:
            - prompt: "b: {{ input }}"
      join: '.a + .b'
"#,
        )
        .expect("workflow with a parallel step should parse");

        let parallel = workflow.steps[0]
            .parallel
            .as_ref()
            .expect("step should have a parallel");
        assert_eq!(parallel.branches.len(), 2);
        assert_eq!(parallel.branches[0].id.as_deref(), Some("a"));
        assert_eq!(parallel.join.as_deref(), Some(".a + .b"));
    }

    #[test]
    fn parses_a_parallel_without_join() {
        let workflow = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - steps:
            - jq: "."
        - steps:
            - jq: "."
"#,
        )
        .expect("workflow with a parallel step without join should parse");

        assert!(workflow.steps[0].parallel.as_ref().unwrap().join.is_none());
    }

    #[test]
    fn parallel_branch_label_defaults_to_branch_n() {
        let workflow = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - steps:
            - jq: "."
        - id: named
          steps:
            - jq: "."
"#,
        )
        .expect("workflow with a parallel step should parse");

        let branches = &workflow.steps[0].parallel.as_ref().unwrap().branches;
        assert_eq!(branches[0].label(0), "branch-1");
        assert_eq!(branches[1].label(1), "named");
    }

    #[test]
    fn rejects_a_parallel_with_empty_branches() {
        let result = parse_workflow(
            r#"
steps:
  - parallel:
      branches: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_parallel_branch_with_empty_steps() {
        let result = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - steps: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_parallel_with_duplicate_branch_ids() {
        let result = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - id: same
          steps:
            - jq: "."
        - id: same
          steps:
            - jq: "."
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_parallel_combined_with_prompt() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    parallel:
      branches:
        - steps:
            - jq: "."
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_parallel_combined_with_input_schema() {
        let result = parse_workflow(
            r#"
steps:
  - input_schema: schema.json
    parallel:
      branches:
        - steps:
            - jq: "."
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_step_with_both_switch_and_parallel() {
        let result = parse_workflow(
            r#"
steps:
  - switch:
      cases:
        - when: 'true'
          steps:
            - jq: "."
    parallel:
      branches:
        - steps:
            - jq: "."
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validates_steps_nested_inside_a_parallel_branch() {
        let result = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - steps:
            - prompt: "{{ input }}"
              agent: agents/extract.md
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_loop_with_while_and_max_iterations() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: refine
    loop:
      while: '.score < 3'
      max_iterations: 5
      steps:
        - jq: '.score += 1'
"#,
        )
        .expect("workflow with a while loop should parse");

        let loop_def = workflow.steps[0]
            .r#loop
            .as_ref()
            .expect("step should have a loop");
        assert_eq!(loop_def.r#while.as_deref(), Some(".score < 3"));
        assert!(loop_def.until.is_none());
        assert_eq!(loop_def.max_iterations, Some(5));
    }

    #[test]
    fn parses_a_loop_with_until() {
        let workflow = parse_workflow(
            r#"
steps:
  - loop:
      until: '.valid == true'
      max_iterations: 3
      steps:
        - jq: '.'
"#,
        )
        .expect("workflow with an until loop should parse");

        let loop_def = workflow.steps[0].r#loop.as_ref().unwrap();
        assert_eq!(loop_def.until.as_deref(), Some(".valid == true"));
        assert!(loop_def.r#while.is_none());
    }

    #[test]
    fn rejects_a_loop_with_both_while_and_until() {
        let result = parse_workflow(
            r#"
steps:
  - loop:
      while: 'true'
      until: 'true'
      max_iterations: 3
      steps:
        - jq: '.'
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_loop_with_neither_while_nor_until() {
        let result = parse_workflow(
            r#"
steps:
  - loop:
      max_iterations: 3
      steps:
        - jq: '.'
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_loop_with_no_max_iterations() {
        let result = parse_workflow(
            r#"
steps:
  - loop:
      until: 'true'
      steps:
        - jq: '.'
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_loop_with_max_iterations_zero() {
        let result = parse_workflow(
            r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 0
      steps:
        - jq: '.'
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_loop_with_empty_steps() {
        let result = parse_workflow(
            r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_loop_combined_with_prompt() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    loop:
      until: 'true'
      max_iterations: 3
      steps:
        - jq: '.'
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validates_steps_nested_inside_a_loop() {
        let result = parse_workflow(
            r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - prompt: "{{ input }}"
          agent: agents/extract.md
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_for_each_with_items_and_join() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: process
    for_each:
      items: '.items'
      steps:
        - jq: '. + 1'
      join: 'map(. * 2)'
"#,
        )
        .expect("workflow with a for_each should parse");

        let for_each = workflow.steps[0]
            .for_each
            .as_ref()
            .expect("step should have a for_each");
        assert_eq!(for_each.items, ".items");
        assert_eq!(for_each.join.as_deref(), Some("map(. * 2)"));
    }

    #[test]
    fn parses_a_for_each_without_join() {
        let workflow = parse_workflow(
            r#"
steps:
  - for_each:
      items: '.items'
      steps:
        - jq: '.'
"#,
        )
        .expect("workflow with a for_each without join should parse");

        assert!(workflow.steps[0].for_each.as_ref().unwrap().join.is_none());
    }

    #[test]
    fn rejects_a_for_each_with_empty_steps() {
        let result = parse_workflow(
            r#"
steps:
  - for_each:
      items: '.items'
      steps: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_for_each_combined_with_when() {
        let result = parse_workflow(
            r#"
steps:
  - when: 'true'
    for_each:
      items: '.items'
      steps:
        - jq: '.'
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validates_steps_nested_inside_a_for_each() {
        let result = parse_workflow(
            r#"
steps:
  - for_each:
      items: '.items'
      steps:
        - prompt: "{{ input }}"
          agent: agents/extract.md
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_step_with_both_loop_and_for_each() {
        let result = parse_workflow(
            r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - jq: '.'
    for_each:
      items: '.items'
      steps:
        - jq: '.'
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_step_with_stop() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: done
    when: '.ready'
    stop: true
"#,
        )
        .expect("workflow with a top-level 'stop' should parse");

        assert_eq!(workflow.steps[0].stop, Some(true));
    }

    #[test]
    fn parses_a_step_with_break_inside_a_loop() {
        let workflow = parse_workflow(
            r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - when: '.done'
          break: true
        - jq: '.'
"#,
        )
        .expect("workflow with 'break' inside a loop should parse");

        let loop_def = workflow.steps[0].r#loop.as_ref().unwrap();
        assert_eq!(loop_def.steps[0].r#break, Some(true));
    }

    #[test]
    fn parses_a_step_with_break_inside_a_for_each() {
        let workflow = parse_workflow(
            r#"
steps:
  - for_each:
      items: '.items'
      steps:
        - when: '.done'
          break: true
        - jq: '.'
"#,
        )
        .expect("workflow with 'break' inside a for_each should parse");

        let for_each = workflow.steps[0].for_each.as_ref().unwrap();
        assert_eq!(for_each.steps[0].r#break, Some(true));
    }

    #[test]
    fn allows_break_inside_a_loop_nested_inside_a_parallel_branch() {
        let result = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - steps:
            - loop:
                until: 'true'
                max_iterations: 3
                steps:
                  - break: true
"#,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_break_at_the_top_level() {
        let result = parse_workflow(
            r#"
steps:
  - break: true
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_break_directly_inside_a_parallel_branch() {
        let result = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - steps:
            - break: true
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_stop_inside_a_parallel_branch() {
        let result = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - steps:
            - stop: true
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_stop_inside_a_loop_nested_inside_a_parallel_branch() {
        let result = parse_workflow(
            r#"
steps:
  - parallel:
      branches:
        - steps:
            - loop:
                until: 'true'
                max_iterations: 3
                steps:
                  - stop: true
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_both_stop_and_break_on_the_same_step() {
        let result = parse_workflow(
            r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - stop: true
          break: true
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_step_with_neither_an_action_nor_stop_or_break() {
        let result = parse_workflow(
            r#"
steps:
  - id: empty
    when: 'true'
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_a_step_with_retry_timeout_and_on_error() {
        let workflow = parse_workflow(
            r#"
steps:
  - id: call
    prompt: "{{ input }}"
    timeout: 30
    retry:
      max_attempts: 3
      delay_seconds: 1
      backoff: 2.0
    on_error:
      steps:
        - jq: '{ fallback: .error }'
"#,
        )
        .expect("workflow with retry/timeout/on_error should parse");

        let step = &workflow.steps[0];
        assert_eq!(step.timeout, Some(30));
        let retry = step.retry.as_ref().unwrap();
        assert_eq!(retry.max_attempts, Some(3));
        assert_eq!(retry.delay_seconds, Some(1));
        assert_eq!(retry.backoff, Some(2.0));
        assert_eq!(step.on_error.as_ref().unwrap().steps.len(), 1);
    }

    #[test]
    fn rejects_a_retry_with_no_max_attempts() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    retry:
      delay_seconds: 1
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_retry_with_max_attempts_zero() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    retry:
      max_attempts: 0
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_timeout_of_zero() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    timeout: 0
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_an_on_error_with_an_empty_steps_list() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    on_error:
      steps: []
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validates_steps_nested_inside_on_error() {
        let result = parse_workflow(
            r#"
steps:
  - prompt: "{{ input }}"
    on_error:
      steps:
        - prompt: "{{ input }}"
          agent: agents/extract.md
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn on_error_inherits_the_failing_steps_loop_context_for_break() {
        let workflow = parse_workflow(
            r#"
steps:
  - loop:
      until: 'true'
      max_iterations: 3
      steps:
        - prompt: "{{ input }}"
          on_error:
            steps:
              - break: true
"#,
        );
        assert!(workflow.is_ok());
    }

    #[test]
    fn rejects_a_switch_combined_with_retry() {
        let result = parse_workflow(
            r#"
steps:
  - retry:
      max_attempts: 3
    switch:
      cases:
        - when: 'true'
          steps:
            - prompt: "{{ input }}"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn eval_when_coerces_plain_text_input_to_a_json_string() {
        assert!(eval_when(". == \"hello\"", "hello").unwrap());
        assert!(!eval_when(". == \"hello\"", "world").unwrap());
    }

    #[test]
    fn eval_when_evaluates_against_parsed_json_input() {
        assert!(eval_when(".flag", r#"{"flag":true}"#).unwrap());
        assert!(!eval_when(".flag", r#"{"flag":false}"#).unwrap());
    }
}
