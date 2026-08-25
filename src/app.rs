use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::ResponseFormat;
use futures_util::{StreamExt, TryStreamExt};

use crate::{
    agent::{self, AgentFile},
    cli::{AgentAction, ChatArgs, Cli, Command, RunArgs},
    cli::{AgentRunArgs, ReasoningEffort},
    config::{self, ConfigFile, ModelMap},
    jq, llm, response, schema, template, workflow,
};

const DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";

pub(crate) async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Run(run_args)) => run_workflow(run_args, cli.no_config).await,
        Some(Command::Agent(agent_command)) => match agent_command.action {
            AgentAction::Run(args) => run_agent(args, cli.no_config).await,
        },
        None => run_chat(cli.chat, cli.no_config).await,
    }
}

/// The model/base-URL/API-key/reasoning-effort settings for a single completion
/// request, after resolving aliases and applying every fallback layer.
struct RequestSettings {
    base_url: String,
    api_key: String,
    resolved_model: config::ResolvedModel,
    reasoning_effort: Option<ReasoningEffort>,
}

impl RequestSettings {
    /// Sends a single completion request built from these settings.
    async fn complete(
        &self,
        system_prompt: Option<&str>,
        prompt: &str,
        response_format: Option<ResponseFormat>,
    ) -> Result<response::ChatCompletionResponse> {
        llm::complete(llm::CompletionRequest {
            base_url: &self.base_url,
            api_key: &self.api_key,
            model_id: &self.resolved_model.model_id,
            reasoning_effort: self.reasoning_effort,
            response_format,
            system_prompt,
            prompt,
        })
        .await
    }
}

/// Renders an agent's system prompt against `input`, calls the model with
/// `prompt` as the user message, and renders the response. Shared by
/// `run_agent` and `execute_step`'s agent branch.
async fn call_agent(
    agent_file: &AgentFile,
    settings: &RequestSettings,
    input: &serde_json::Value,
    prompt: &str,
    steps_outputs: &workflow::StepOutputs,
) -> Result<String> {
    let system_prompt = template::render(&agent_file.system_prompt_template, input, steps_outputs)?;
    let response_format = agent_file
        .structured_output
        .then(|| {
            schema::build_response_format_from_entry(
                agent_file.output_schema.as_ref().expect(
                    "load_agent validates structured_output implies output_schema is present",
                ),
                agent_file.schema_name(),
            )
        })
        .transpose()?;

    let response = settings
        .complete(Some(&system_prompt), prompt, response_format)
        .await?;
    response::render_response(&response, false, false)
}

/// Resolves the settings for one completion request. `model_name` and
/// `reasoning_effort` must already reflect the caller's own precedence chain
/// (e.g. step > agent > workflow default); this only adds the two layers every
/// caller shares: the resolved model's own defaults, then `lait.config.yml`'s
/// `default:` block. `local_models` is the alias map to check before falling
/// back to `file_config`'s (a workflow's embedded `models:`, or empty when
/// there is none).
fn resolve_request_settings(
    model_name: String,
    reasoning_effort: Option<ReasoningEffort>,
    base_url_override: Option<String>,
    api_key_override: Option<String>,
    local_models: &ModelMap,
    file_config: &ConfigFile,
) -> Result<RequestSettings> {
    let resolved_model = match config::resolve_model_alias(&model_name, local_models)? {
        Some(resolved) => resolved,
        None => config::resolve_model(model_name, file_config)?,
    };
    let base_url = base_url_override
        .or_else(|| resolved_model.base_url.clone())
        .or_else(|| file_config.base_url.clone())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let base_url = base_url.trim_end_matches('/').to_owned();
    if base_url.is_empty() {
        return Err(anyhow!("base URL must not be empty"));
    }
    let api_key = api_key_override
        .or_else(|| resolved_model.api_key.clone())
        .or_else(|| file_config.api_key.clone())
        .unwrap_or_else(|| {
            // async-openai always builds an Authorization header from its config.
            // LM Studio ignores the value, so use a non-empty dummy key when no
            // key was supplied instead of making local requests fail on an empty
            // header value.
            "lm-studio".to_owned()
        });
    let reasoning_effort = reasoning_effort
        .or(resolved_model.reasoning_effort)
        .or(file_config.default.reasoning_effort);

    Ok(RequestSettings {
        base_url,
        api_key,
        resolved_model,
        reasoning_effort,
    })
}

async fn run_chat(chat: ChatArgs, no_config: bool) -> Result<()> {
    let prompt = chat.prompt.clone().ok_or_else(|| {
        anyhow!("a PROMPT is required; provide one, or use `lait run <FILE> <PROMPT>`")
    })?;

    let file_config = config::load_config(no_config)?;
    let model_name = chat
        .model
        .clone()
        .or_else(|| file_config.default.model.clone())
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "model is required; provide --model, set LLM_MODEL, or specify default.model in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
    let settings = resolve_request_settings(
        model_name,
        chat.reasoning_effort,
        chat.base_url.clone(),
        chat.api_key.clone(),
        &ModelMap::default(),
        &file_config,
    )?;

    let response_format = chat
        .json_schema
        .as_deref()
        .map(|path| schema::load_json_schema(path, &chat.schema_name))
        .transpose()?;

    let response = settings.complete(None, &prompt, response_format).await?;

    let output = response::render_response(&response, chat.json, chat.show_reasoning)?;
    println!("{output}");
    Ok(())
}

async fn run_agent(args: AgentRunArgs, no_config: bool) -> Result<()> {
    let agent_file = agent::load_agent(&args.file)?;
    let file_config = config::load_config(no_config)?;

    if let Some(name) = &agent_file.name {
        match &agent_file.description {
            Some(description) => eprintln!("==> {name}: {description}"),
            None => eprintln!("==> {name}"),
        }
    }

    let input = template::parse_input(&args.input);
    agent_file
        .validate_input(&input)
        .with_context(|| format!("agent '{}'", args.file.display()))?;

    let model_name = agent_file
        .model
        .clone()
        .or_else(|| file_config.default.model.clone())
        .ok_or_else(|| {
            anyhow!(
                "model is required; set it in the agent frontmatter or default.model in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
    let settings = resolve_request_settings(
        model_name,
        agent_file.reasoning_effort,
        None,
        None,
        &ModelMap::default(),
        &file_config,
    )?;

    let output = call_agent(
        &agent_file,
        &settings,
        &input,
        &args.input,
        &workflow::StepOutputs::new(),
    )
    .await
    .with_context(|| format!("agent '{}'", args.file.display()))?;
    println!("{output}");
    Ok(())
}

async fn run_workflow(run_args: RunArgs, no_config: bool) -> Result<()> {
    let wf = workflow::load_workflow(&run_args.file)?;
    let file_config = config::load_config(no_config)?;

    if let Some(name) = &wf.name {
        match &wf.description {
            Some(description) => eprintln!("==> {name}: {description}"),
            None => eprintln!("==> {name}"),
        }
    }

    let scope = WorkflowScope::top_level(&wf, &run_args.file)?;
    let (current_input, _, _, _) = run_steps(
        &wf.steps,
        run_args.prompt,
        &scope,
        &file_config,
        0,
        "",
        workflow::StepOutputs::new(),
    )
    .await?;
    println!("{current_input}");
    Ok(())
}

/// The maximum `workflow:` nesting depth (a workflow step calling another
/// workflow file, whose own steps may call another, ...), rejected as a
/// runtime error rather than left to overflow the stack or hang.
const MAX_WORKFLOW_DEPTH: usize = 32;

/// The default model/reasoning-effort, model aliases, and JSON schema
/// definitions currently in effect, plus enough bookkeeping to run a nested
/// `workflow:` step safely. Every `resolve_step_settings`/`execute_step` call
/// reads through this instead of a `&workflow::WorkflowFile` directly, so a
/// `workflow:` step's sub-workflow can see its own `default:`/`models:`/
/// `json_schemas:` first, falling back to its caller's (`nested` builds
/// that merge). `active_paths` records every workflow file currently
/// executing (canonicalized), to reject a `workflow:` cycle and to cap
/// nesting depth at `MAX_WORKFLOW_DEPTH`.
struct WorkflowScope {
    default_model: Option<String>,
    default_reasoning_effort: Option<ReasoningEffort>,
    models: ModelMap,
    json_schemas: schema::JsonSchemaMap,
    /// Directory relative paths in this scope's workflow file (currently
    /// only `step.workflow`) are resolved against.
    base_dir: PathBuf,
    active_paths: Vec<PathBuf>,
}

impl WorkflowScope {
    /// The scope for the workflow file passed on the command line.
    fn top_level(wf: &workflow::WorkflowFile, file_path: &Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(file_path).with_context(|| {
            format!(
                "failed to resolve workflow file path '{}'",
                file_path.display()
            )
        })?;
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            default_model: wf.default.model.clone(),
            default_reasoning_effort: wf.default.reasoning_effort,
            models: wf.models.clone(),
            json_schemas: wf.json_schemas.clone(),
            base_dir,
            active_paths: vec![canonical],
        })
    }

    /// The scope for a `workflow:` step's sub-workflow: resolves
    /// `relative_path` (as given in the step) against this scope's
    /// `base_dir`, merges `sub_wf`'s `default`/`models`/`json_schemas` over
    /// this scope's (the sub-workflow's own entries win; an entry it doesn't
    /// define falls back to this scope's), and extends the cycle/depth
    /// bookkeeping. Fails if `relative_path` resolves to a workflow file
    /// already executing (a cycle) or nesting has reached
    /// `MAX_WORKFLOW_DEPTH`.
    fn nested(
        &self,
        relative_path: &Path,
        sub_wf: &workflow::WorkflowFile,
        label: &str,
    ) -> Result<Self> {
        let resolved_path = self.base_dir.join(relative_path);
        let canonical = std::fs::canonicalize(&resolved_path).with_context(|| {
            format!(
                "step '{label}': failed to resolve workflow file path '{}'",
                resolved_path.display()
            )
        })?;
        if self.active_paths.contains(&canonical) {
            bail!(
                "step '{label}': 'workflow: {}' would create a cycle ('{}' is already running)",
                relative_path.display(),
                canonical.display()
            );
        }
        if self.active_paths.len() >= MAX_WORKFLOW_DEPTH {
            bail!(
                "step '{label}': 'workflow:' nesting exceeded the maximum depth of {MAX_WORKFLOW_DEPTH}"
            );
        }

        let mut models = sub_wf.models.clone();
        for (name, definitions) in &self.models {
            models
                .entry(name.clone())
                .or_insert_with(|| definitions.clone());
        }
        let mut json_schemas = sub_wf.json_schemas.clone();
        for (name, entry) in &self.json_schemas {
            json_schemas
                .entry(name.clone())
                .or_insert_with(|| entry.clone());
        }
        let mut active_paths = self.active_paths.clone();
        active_paths.push(canonical.clone());
        let base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(Self {
            default_model: sub_wf
                .default
                .model
                .clone()
                .or_else(|| self.default_model.clone()),
            default_reasoning_effort: sub_wf
                .default
                .reasoning_effort
                .or(self.default_reasoning_effort),
            models,
            json_schemas,
            base_dir,
            active_paths,
        })
    }
}

/// Sets `steps_outputs[step.id]` to `output` (JSON-parsed, like a `parallel`
/// branch's output before joining), for a step with an explicit `id`. A step
/// without one keeps its auto-generated `step-N` progress label out of
/// `{{ steps.* }}`/`$steps`, since that label isn't a stable name to reference.
fn record_step_output(
    steps_outputs: &mut workflow::StepOutputs,
    step: &workflow::StepDefinition,
    output: &str,
) {
    if let Some(id) = &step.id {
        steps_outputs.insert(id.clone(), template::parse_input(output));
    }
}

/// A signal returned by `run_steps`, alongside its final input and progress
/// counter, describing how the run ended: `Continue` is the normal
/// end-of-list case; `Break`/`Stop` come from a `break: true`/`stop: true`
/// step (see `workflow::StepDefinition`) and bubble up through `switch`/
/// `loop`/`for_each` frames until something catches them (`loop`/`for_each`
/// catch `Break`; nothing but `run_workflow` catches `Stop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Break,
    Stop,
}

/// The final input, the running progress counter, the `Flow` signal the run
/// ended with, and the named step outputs recorded along the way, returned by
/// `run_steps`.
type StepsOutcome = Result<(String, usize, Flow, workflow::StepOutputs)>;

/// Runs a sequence of steps (the workflow's top-level `steps`, the nested
/// `steps` of a `switch` case/`else`, or a `parallel` branch), returning the
/// final input and the running progress counter so nested calls keep
/// numbering `[n]` labels continuously across the whole executed path
/// (skipped steps still consume a number). `progress_prefix` is prepended to
/// every progress line, so a `parallel` branch's interleaved output stays
/// attributable to its branch; it is threaded through unchanged by `switch`
/// (only one case ever runs, so its numbering stays continuous with the
/// parent) but reset to a fresh branch-local prefix and counter by
/// `parallel` (every branch runs concurrently, so a single shared counter
/// would not reflect real execution order). `steps_outputs` is threaded the
/// same way as `current_input`/`counter` for a `switch` case, `loop`
/// iteration, or `for_each` item (each sees every id recorded so far, and its
/// own recordings flow to whatever runs after it), but is only ever cloned
/// into a `parallel` branch, never merged back: concurrently running branches
/// recording into a shared namespace would race, and there is no well-defined
/// "the" value for an id set differently by two branches. Boxed because a
/// `switch`/`parallel` step recurses into this function from within an
/// `async` body, which Rust cannot size otherwise.
fn run_steps<'a>(
    steps: &'a [workflow::StepDefinition],
    current_input: String,
    scope: &'a WorkflowScope,
    file_config: &'a ConfigFile,
    start_counter: usize,
    progress_prefix: &'a str,
    steps_outputs: workflow::StepOutputs,
) -> Pin<Box<dyn Future<Output = StepsOutcome> + 'a>> {
    Box::pin(async move {
        let mut current_input = current_input;
        let mut counter = start_counter;
        let mut steps_outputs = steps_outputs;
        for step in steps {
            counter += 1;
            let label = step.id.clone().unwrap_or_else(|| format!("step-{counter}"));

            if let Some(switch) = &step.switch {
                eprintln!("{progress_prefix}[{counter}] {label}");

                let mut matched = None;
                for (case_index, case) in switch.cases.iter().enumerate() {
                    if workflow::eval_when(&case.when, &current_input, &steps_outputs)
                        .with_context(|| format!("step '{label}'"))?
                    {
                        let case_label = case
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("case-{}", case_index + 1));
                        eprintln!("{progress_prefix}    -> case '{case_label}' matched");
                        matched = Some(
                            run_steps(
                                &case.steps,
                                current_input.clone(),
                                scope,
                                file_config,
                                counter,
                                progress_prefix,
                                steps_outputs.clone(),
                            )
                            .await?,
                        );
                        break;
                    }
                }
                let (result, new_counter, flow, new_steps_outputs) = match matched {
                    Some(result) => result,
                    None => match &switch.else_steps {
                        Some(else_steps) => {
                            eprintln!("{progress_prefix}    -> no case matched, running 'else'");
                            run_steps(
                                else_steps,
                                current_input.clone(),
                                scope,
                                file_config,
                                counter,
                                progress_prefix,
                                steps_outputs.clone(),
                            )
                            .await?
                        }
                        None => {
                            bail!("step '{label}': no case matched and no 'else' branch is defined")
                        }
                    },
                };
                current_input = result;
                counter = new_counter;
                steps_outputs = new_steps_outputs;
                record_step_output(&mut steps_outputs, step, &current_input);
                if flow != Flow::Continue {
                    return Ok((current_input, counter, flow, steps_outputs));
                }
                continue;
            }

            if let Some(parallel) = &step.parallel {
                eprintln!("{progress_prefix}[{counter}] {label}");
                eprintln!(
                    "{progress_prefix}    -> running {} branches concurrently",
                    parallel.branches.len()
                );

                let branch_labels: Vec<String> = parallel
                    .branches
                    .iter()
                    .enumerate()
                    .map(|(index, branch)| branch.label(index))
                    .collect();
                let branch_prefixes: Vec<String> = branch_labels
                    .iter()
                    .map(|branch_label| format!("{progress_prefix}[{branch_label}] "))
                    .collect();
                let branch_futures = parallel.branches.iter().zip(&branch_prefixes).map(
                    |(branch, branch_prefix)| {
                        run_steps(
                            &branch.steps,
                            current_input.clone(),
                            scope,
                            file_config,
                            0,
                            branch_prefix,
                            steps_outputs.clone(),
                        )
                    },
                );
                let branch_results = futures_util::future::try_join_all(branch_futures).await?;

                // `validate_steps` rejects `stop`/`break` anywhere inside a
                // `parallel` branch, so every branch always finishes with
                // `Flow::Continue`; only its output is used here. Each branch
                // got its own clone of `steps_outputs` (see this function's
                // doc comment), so whatever it recorded stays branch-local.
                let mut joined = serde_json::Map::new();
                for (branch_label, (branch_output, _, _, _)) in
                    branch_labels.into_iter().zip(branch_results)
                {
                    joined.insert(branch_label, template::parse_input(&branch_output));
                }
                let joined_json = serde_json::to_string(&serde_json::Value::Object(joined))
                    .context("failed to serialize joined 'parallel' branch outputs")?;

                eprintln!("{progress_prefix}    -> branches joined");

                current_input = match &parallel.join {
                    Some(filter) => jq::apply(filter, &joined_json, &steps_outputs)
                        .with_context(|| format!("step '{label}'"))?,
                    None => joined_json,
                };
                record_step_output(&mut steps_outputs, step, &current_input);
                continue;
            }

            if let Some(loop_def) = &step.r#loop {
                eprintln!("{progress_prefix}[{counter}] {label}");
                // Validated by `workflow::validate_steps`: exactly one of
                // `while`/`until` is set, and `max_iterations` is `Some(n)` with n >= 1.
                let max_iterations = loop_def
                    .max_iterations
                    .expect("loop.max_iterations is required by validate_steps");

                let mut iteration_input = current_input.clone();
                // Threaded continuously across iterations (like `switch`, unlike
                // `parallel`'s per-branch reset): the loop body genuinely runs
                // sequentially, so a single growing counter reflects real execution
                // order.
                let mut loop_counter = counter;
                if let Some(while_cond) = &loop_def.r#while {
                    let mut iterations_run = 0usize;
                    loop {
                        let should_continue =
                            workflow::eval_when(while_cond, &iteration_input, &steps_outputs)
                                .with_context(|| format!("step '{label}'"))?;
                        if !should_continue {
                            break;
                        }
                        if iterations_run >= max_iterations {
                            bail!(
                                "step '{label}': 'loop' reached max_iterations ({max_iterations}) without satisfying 'while'"
                            );
                        }
                        iterations_run += 1;
                        eprintln!(
                            "{progress_prefix}    -> iteration {iterations_run}/{max_iterations}"
                        );
                        let (result, new_counter, flow, new_steps_outputs) = run_steps(
                            &loop_def.steps,
                            iteration_input.clone(),
                            scope,
                            file_config,
                            loop_counter,
                            progress_prefix,
                            steps_outputs.clone(),
                        )
                        .await?;
                        iteration_input = result;
                        loop_counter = new_counter;
                        steps_outputs = new_steps_outputs;
                        match flow {
                            Flow::Continue => {}
                            Flow::Break => break,
                            Flow::Stop => {
                                return Ok((
                                    iteration_input,
                                    loop_counter,
                                    Flow::Stop,
                                    steps_outputs,
                                ));
                            }
                        }
                    }
                } else {
                    let until_cond = loop_def
                        .until
                        .as_ref()
                        .expect("loop.until is required by validate_steps when 'while' is unset");
                    let mut iterations_run = 0usize;
                    let mut satisfied = false;
                    while iterations_run < max_iterations {
                        iterations_run += 1;
                        eprintln!(
                            "{progress_prefix}    -> iteration {iterations_run}/{max_iterations}"
                        );
                        let (result, new_counter, flow, new_steps_outputs) = run_steps(
                            &loop_def.steps,
                            iteration_input.clone(),
                            scope,
                            file_config,
                            loop_counter,
                            progress_prefix,
                            steps_outputs.clone(),
                        )
                        .await?;
                        iteration_input = result;
                        loop_counter = new_counter;
                        steps_outputs = new_steps_outputs;
                        if flow == Flow::Stop {
                            return Ok((iteration_input, loop_counter, Flow::Stop, steps_outputs));
                        }
                        if flow == Flow::Break {
                            // An explicit `break: true` ends the loop like a
                            // satisfied `until`, not like exhausting
                            // `max_iterations`.
                            satisfied = true;
                            break;
                        }
                        satisfied =
                            workflow::eval_when(until_cond, &iteration_input, &steps_outputs)
                                .with_context(|| format!("step '{label}'"))?;
                        if satisfied {
                            break;
                        }
                    }
                    if !satisfied {
                        bail!(
                            "step '{label}': 'loop' reached max_iterations ({max_iterations}) without satisfying 'until'"
                        );
                    }
                }
                current_input = iteration_input;
                counter = loop_counter;
                record_step_output(&mut steps_outputs, step, &current_input);
                continue;
            }

            if let Some(for_each) = &step.for_each {
                eprintln!("{progress_prefix}[{counter}] {label}");
                let items_json = jq::apply_one(&for_each.items, &current_input, &steps_outputs)
                    .with_context(|| format!("step '{label}'"))?;
                let items_value: serde_json::Value = serde_json::from_str(&items_json)
                    .with_context(|| {
                        format!("step '{label}': failed to parse 'for_each.items' output as JSON")
                    })?;
                let items = items_value.as_array().cloned().ok_or_else(|| {
                    anyhow!("step '{label}': 'for_each.items' must produce a JSON array")
                })?;

                let max_concurrency = for_each.max_concurrency.unwrap_or(1);
                let results: Vec<serde_json::Value> = if max_concurrency <= 1 {
                    eprintln!(
                        "{progress_prefix}    -> iterating over {} item(s)",
                        items.len()
                    );
                    let mut results = Vec::with_capacity(items.len());
                    // Threaded continuously across items, like `loop` (see its
                    // comment above): a sequential `for_each` (the default)
                    // runs its body one item at a time, so a single growing
                    // counter matches real execution order.
                    let mut for_each_counter = counter;
                    let mut stop_result = None;
                    for (item_index, item) in items.iter().enumerate() {
                        eprintln!(
                            "{progress_prefix}    -> item {}/{}",
                            item_index + 1,
                            items.len()
                        );
                        // A string item is passed through raw (like `parallel`'s
                        // `current_input`, and the inverse of `template::parse_input`
                        // used below for results), not re-quoted as JSON, so
                        // `{{ input }}` sees the same unquoted text everywhere else
                        // in the pipeline does.
                        let item_input = match item {
                            serde_json::Value::String(text) => text.clone(),
                            other => serde_json::to_string(other)
                                .context("failed to serialize a 'for_each' item")?,
                        };
                        let (result, new_counter, flow, new_steps_outputs) = run_steps(
                            &for_each.steps,
                            item_input,
                            scope,
                            file_config,
                            for_each_counter,
                            progress_prefix,
                            steps_outputs.clone(),
                        )
                        .await?;
                        for_each_counter = new_counter;
                        steps_outputs = new_steps_outputs;
                        if flow == Flow::Stop {
                            stop_result = Some(result);
                            break;
                        }
                        results.push(template::parse_input(&result));
                        if flow == Flow::Break {
                            break;
                        }
                    }
                    counter = for_each_counter;
                    if let Some(result) = stop_result {
                        return Ok((result, counter, Flow::Stop, steps_outputs));
                    }
                    results
                } else {
                    eprintln!(
                        "{progress_prefix}    -> iterating over {} item(s), up to {max_concurrency} concurrently",
                        items.len()
                    );
                    let item_inputs: Vec<String> = items
                        .iter()
                        .map(|item| match item {
                            serde_json::Value::String(text) => Ok(text.clone()),
                            other => serde_json::to_string(other)
                                .context("failed to serialize a 'for_each' item"),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let item_prefixes: Vec<String> = (0..item_inputs.len())
                        .map(|index| format!("{progress_prefix}[item-{}] ", index + 1))
                        .collect();
                    let item_futures = item_inputs.into_iter().zip(&item_prefixes).map(
                        |(item_input, item_prefix)| {
                            run_steps(
                                &for_each.steps,
                                item_input,
                                scope,
                                file_config,
                                0,
                                item_prefix,
                                steps_outputs.clone(),
                            )
                        },
                    );
                    // `validate_steps` rejects `stop`/`break` inside a
                    // `for_each` body whose `max_concurrency` is above 1, for
                    // the same reason as a `parallel` branch: concurrently
                    // running items can't share a single well-defined "break
                    // this loop"/"stop the workflow" target. Each item also
                    // got its own clone of `steps_outputs` (see this
                    // function's doc comment), so nothing it records leaks
                    // back here.
                    let item_results: Vec<(String, usize, Flow, workflow::StepOutputs)> =
                        futures_util::stream::iter(item_futures)
                            .buffered(max_concurrency)
                            .try_collect()
                            .await?;
                    item_results
                        .into_iter()
                        .map(|(output, _, _, _)| template::parse_input(&output))
                        .collect()
                };

                let results_json = serde_json::to_string(&serde_json::Value::Array(results))
                    .context("failed to serialize 'for_each' results")?;

                current_input = match &for_each.join {
                    Some(filter) => jq::apply(filter, &results_json, &steps_outputs)
                        .with_context(|| format!("step '{label}'"))?,
                    None => results_json,
                };
                record_step_output(&mut steps_outputs, step, &current_input);
                continue;
            }

            if let Some(when) = &step.when {
                let truthy = workflow::eval_when(when, &current_input, &steps_outputs)
                    .with_context(|| format!("step '{label}'"))?;
                if !truthy {
                    eprintln!("{progress_prefix}[{counter}] {label} (skipped)");
                    continue;
                }
            }

            eprintln!("{progress_prefix}[{counter}] {label}");
            let attempt_result = execute_step_with_retry(
                step,
                &current_input,
                scope,
                file_config,
                &label,
                progress_prefix,
                &steps_outputs,
            )
            .await;
            current_input = match attempt_result {
                Ok(output) => output,
                Err(error) => match &step.on_error {
                    Some(on_error) => {
                        eprintln!(
                            "{progress_prefix}    -> step failed, running 'on_error': {error}"
                        );
                        let error_input = serde_json::json!({
                            "error": error.to_string(),
                            "input": template::parse_input(&current_input),
                        });
                        let error_input_json = serde_json::to_string(&error_input)
                            .context("failed to serialize 'on_error' input")?;
                        let (result, new_counter, flow, new_steps_outputs) = run_steps(
                            &on_error.steps,
                            error_input_json,
                            scope,
                            file_config,
                            counter,
                            progress_prefix,
                            steps_outputs.clone(),
                        )
                        .await?;
                        counter = new_counter;
                        steps_outputs = new_steps_outputs;
                        if flow != Flow::Continue {
                            return Ok((result, counter, flow, steps_outputs));
                        }
                        result
                    }
                    None => return Err(error),
                },
            };

            record_step_output(&mut steps_outputs, step, &current_input);

            if step.r#break == Some(true) {
                return Ok((current_input, counter, Flow::Break, steps_outputs));
            }
            if step.stop == Some(true) {
                return Ok((current_input, counter, Flow::Stop, steps_outputs));
            }
        }
        Ok((current_input, counter, Flow::Continue, steps_outputs))
    })
}

/// Runs `execute_step`, applying `step.timeout` to each attempt and retrying
/// per `step.retry` on failure (a timed-out attempt counts as a failure).
/// Returns the last attempt's error once `retry.max_attempts` (or 1, with no
/// `retry`) is exhausted; the caller decides whether to run `on_error` or
/// propagate it.
async fn execute_step_with_retry(
    step: &workflow::StepDefinition,
    current_input: &str,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    label: &str,
    progress_prefix: &str,
    steps_outputs: &workflow::StepOutputs,
) -> Result<String> {
    let max_attempts = step
        .retry
        .as_ref()
        .and_then(|retry| retry.max_attempts)
        .unwrap_or(1);
    let backoff = step
        .retry
        .as_ref()
        .and_then(|retry| retry.backoff)
        .unwrap_or(1.0);
    let mut delay = Duration::from_secs(
        step.retry
            .as_ref()
            .and_then(|retry| retry.delay_seconds)
            .unwrap_or(0),
    );

    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let outcome = match step.timeout {
            Some(seconds) => {
                match tokio::time::timeout(
                    Duration::from_secs(seconds),
                    execute_step(
                        step,
                        current_input,
                        scope,
                        file_config,
                        label,
                        progress_prefix,
                        steps_outputs,
                    ),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!(
                        "step '{label}' timed out after {seconds}s (attempt {attempt}/{max_attempts})"
                    )),
                }
            }
            None => {
                execute_step(
                    step,
                    current_input,
                    scope,
                    file_config,
                    label,
                    progress_prefix,
                    steps_outputs,
                )
                .await
            }
        };

        match outcome {
            Ok(output) => return Ok(output),
            Err(error) if attempt < max_attempts => {
                eprintln!(
                    "{progress_prefix}    -> attempt {attempt}/{max_attempts} failed: {error}; retrying in {:.1}s",
                    delay.as_secs_f64()
                );
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                delay = Duration::from_secs_f64((delay.as_secs_f64() * backoff).max(0.0));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Resolves the model/reasoning-effort settings for a step's model call,
/// applying the step > agent file (when this step has one) > workflow
/// default precedence chain shared by `execute_step`'s `agent` and `prompt`
/// branches. `agent_file` is `Some` only for an `agent` step; besides adding
/// its fallback layer, its presence also selects which hint text a
/// missing-model error uses.
fn resolve_step_settings(
    step: &workflow::StepDefinition,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    agent_file: Option<&AgentFile>,
    label: &str,
) -> Result<RequestSettings> {
    let model_name = step
        .model
        .clone()
        .or_else(|| agent_file.and_then(|agent_file| agent_file.model.clone()))
        .or_else(|| scope.default_model.clone())
        .ok_or_else(|| {
            anyhow!(
                "model is required for step '{label}'; set it on the step,{} the workflow's default.model, or in {}",
                if agent_file.is_some() { " its agent file," } else { "" },
                config::CONFIG_FILE_NAME
            )
        })?;
    let reasoning_effort = step
        .reasoning_effort
        .or(agent_file.and_then(|agent_file| agent_file.reasoning_effort))
        .or(scope.default_reasoning_effort);
    resolve_request_settings(
        model_name,
        reasoning_effort,
        None,
        None,
        &scope.models,
        file_config,
    )
    .with_context(|| format!("step '{label}'"))
}

/// Runs a single non-`switch` step (agent call, prompt call, or `jq`-only
/// data transform) and returns its output, with `jq` applied afterward if set.
async fn execute_step(
    step: &workflow::StepDefinition,
    current_input: &str,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    label: &str,
    progress_prefix: &str,
    steps_outputs: &workflow::StepOutputs,
) -> Result<String> {
    if let Some(name_or_path) = &step.input_schema {
        let schema = schema::resolve_named_schema_value(&scope.json_schemas, name_or_path)
            .with_context(|| format!("step '{label}'"))?;
        let input = template::parse_input(current_input);
        schema::validate_input_against_schema(&schema, &input)
            .with_context(|| format!("step '{label}'"))?;
    }

    let mut step_output = if let Some(agent_path) = &step.agent {
        let agent_file =
            agent::load_agent(agent_path).with_context(|| format!("step '{label}'"))?;

        let input = template::parse_input(current_input);
        agent_file
            .validate_input(&input)
            .with_context(|| format!("step '{label}'"))?;

        let settings = resolve_step_settings(step, scope, file_config, Some(&agent_file), label)?;

        call_agent(&agent_file, &settings, &input, current_input, steps_outputs)
            .await
            .with_context(|| format!("step '{label}'"))?
    } else if let Some(prompt_template) = &step.prompt {
        let settings = resolve_step_settings(step, scope, file_config, None, label)?;

        let response_format = step
            .output_schema
            .as_deref()
            .map(|name_or_path| {
                let schema_name = step.schema_name.as_deref().unwrap_or("structured_output");
                match scope.json_schemas.get(name_or_path) {
                    Some(entry) => schema::build_response_format_from_entry(entry, schema_name),
                    None => {
                        schema::load_json_schema(std::path::Path::new(name_or_path), schema_name)
                    }
                }
            })
            .transpose()
            .with_context(|| format!("step '{label}'"))?;

        let input = template::parse_input(current_input);
        let prompt = template::render(prompt_template, &input, steps_outputs)
            .with_context(|| format!("step '{label}'"))?;

        let response = settings
            .complete(None, &prompt, response_format)
            .await
            .with_context(|| format!("step '{label}'"))?;

        response::render_response(&response, false, false)
            .with_context(|| format!("step '{label}'"))?
    } else if let Some(sub_workflow_path) = &step.workflow {
        let resolved_path = scope.base_dir.join(sub_workflow_path);
        let sub_wf =
            workflow::load_workflow(&resolved_path).with_context(|| format!("step '{label}'"))?;
        let sub_scope = scope.nested(sub_workflow_path, &sub_wf, label)?;
        if let Some(name) = &sub_wf.name {
            match &sub_wf.description {
                Some(description) => eprintln!("{progress_prefix}    -> {name}: {description}"),
                None => eprintln!("{progress_prefix}    -> {name}"),
            }
        }
        // Isolated like an `agent:` call, not threaded like a `switch` case:
        // the sub-workflow is a separate file with its own step ids, so it
        // starts with an empty `steps_outputs` and its Flow (whether it
        // ended via `stop`/`break` internally or just ran out of steps) is
        // this step's own concern, not the caller's — only its final output
        // crosses back.
        let sub_progress_prefix = format!("{progress_prefix}    ");
        let (result, ..) = run_steps(
            &sub_wf.steps,
            current_input.to_string(),
            &sub_scope,
            file_config,
            0,
            &sub_progress_prefix,
            workflow::StepOutputs::new(),
        )
        .await
        .with_context(|| format!("step '{label}'"))?;
        result
    } else {
        current_input.to_string()
    };

    if let Some(filter) = &step.jq {
        step_output = jq::apply(filter, &step_output, steps_outputs)
            .with_context(|| format!("step '{label}'"))?;
    }

    Ok(step_output)
}
