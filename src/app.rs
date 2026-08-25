use std::{future::Future, pin::Pin};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::ResponseFormat;

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
) -> Result<String> {
    let system_prompt = template::render(&agent_file.system_prompt_template, input)?;
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

    let output = call_agent(&agent_file, &settings, &input, &args.input)
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

    let (current_input, _) =
        run_steps(&wf.steps, run_args.prompt, &wf, &file_config, 0, "").await?;
    println!("{current_input}");
    Ok(())
}

/// The final input and the running progress counter, returned by `run_steps`.
type StepsOutcome = Result<(String, usize)>;

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
/// would not reflect real execution order). Boxed because a `switch`/
/// `parallel` step recurses into this function from within an `async` body,
/// which Rust cannot size otherwise.
fn run_steps<'a>(
    steps: &'a [workflow::StepDefinition],
    current_input: String,
    wf: &'a workflow::WorkflowFile,
    file_config: &'a ConfigFile,
    start_counter: usize,
    progress_prefix: &'a str,
) -> Pin<Box<dyn Future<Output = StepsOutcome> + 'a>> {
    Box::pin(async move {
        let mut current_input = current_input;
        let mut counter = start_counter;
        for step in steps {
            counter += 1;
            let label = step.id.clone().unwrap_or_else(|| format!("step-{counter}"));

            if let Some(switch) = &step.switch {
                eprintln!("{progress_prefix}[{counter}] {label}");

                let mut matched = None;
                for (case_index, case) in switch.cases.iter().enumerate() {
                    if workflow::eval_when(&case.when, &current_input)
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
                                wf,
                                file_config,
                                counter,
                                progress_prefix,
                            )
                            .await?,
                        );
                        break;
                    }
                }
                let (result, new_counter) = match matched {
                    Some(result) => result,
                    None => match &switch.else_steps {
                        Some(else_steps) => {
                            eprintln!("{progress_prefix}    -> no case matched, running 'else'");
                            run_steps(
                                else_steps,
                                current_input.clone(),
                                wf,
                                file_config,
                                counter,
                                progress_prefix,
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
                            wf,
                            file_config,
                            0,
                            branch_prefix,
                        )
                    },
                );
                let branch_results = futures_util::future::try_join_all(branch_futures).await?;

                let mut joined = serde_json::Map::new();
                for (branch_label, (branch_output, _)) in
                    branch_labels.into_iter().zip(branch_results)
                {
                    joined.insert(branch_label, template::parse_input(&branch_output));
                }
                let joined_json = serde_json::to_string(&serde_json::Value::Object(joined))
                    .context("failed to serialize joined 'parallel' branch outputs")?;

                eprintln!("{progress_prefix}    -> branches joined");

                current_input = match &parallel.join {
                    Some(filter) => jq::apply(filter, &joined_json)
                        .with_context(|| format!("step '{label}'"))?,
                    None => joined_json,
                };
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
                        let should_continue = workflow::eval_when(while_cond, &iteration_input)
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
                        let (result, new_counter) = run_steps(
                            &loop_def.steps,
                            iteration_input.clone(),
                            wf,
                            file_config,
                            loop_counter,
                            progress_prefix,
                        )
                        .await?;
                        iteration_input = result;
                        loop_counter = new_counter;
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
                        let (result, new_counter) = run_steps(
                            &loop_def.steps,
                            iteration_input.clone(),
                            wf,
                            file_config,
                            loop_counter,
                            progress_prefix,
                        )
                        .await?;
                        iteration_input = result;
                        loop_counter = new_counter;
                        satisfied = workflow::eval_when(until_cond, &iteration_input)
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
                continue;
            }

            if let Some(for_each) = &step.for_each {
                eprintln!("{progress_prefix}[{counter}] {label}");
                let items_json = jq::apply_one(&for_each.items, &current_input)
                    .with_context(|| format!("step '{label}'"))?;
                let items_value: serde_json::Value = serde_json::from_str(&items_json)
                    .with_context(|| {
                        format!("step '{label}': failed to parse 'for_each.items' output as JSON")
                    })?;
                let items = items_value.as_array().cloned().ok_or_else(|| {
                    anyhow!("step '{label}': 'for_each.items' must produce a JSON array")
                })?;
                eprintln!(
                    "{progress_prefix}    -> iterating over {} item(s)",
                    items.len()
                );

                let mut results = Vec::with_capacity(items.len());
                // Threaded continuously across items, like `loop` (see its comment
                // above): `for_each` runs sequentially, not concurrently like
                // `parallel`, so a single growing counter matches real execution order.
                let mut for_each_counter = counter;
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
                    let (result, new_counter) = run_steps(
                        &for_each.steps,
                        item_input,
                        wf,
                        file_config,
                        for_each_counter,
                        progress_prefix,
                    )
                    .await?;
                    for_each_counter = new_counter;
                    results.push(template::parse_input(&result));
                }
                counter = for_each_counter;
                let results_json = serde_json::to_string(&serde_json::Value::Array(results))
                    .context("failed to serialize 'for_each' results")?;

                current_input = match &for_each.join {
                    Some(filter) => jq::apply(filter, &results_json)
                        .with_context(|| format!("step '{label}'"))?,
                    None => results_json,
                };
                continue;
            }

            if let Some(when) = &step.when {
                let truthy = workflow::eval_when(when, &current_input)
                    .with_context(|| format!("step '{label}'"))?;
                if !truthy {
                    eprintln!("{progress_prefix}[{counter}] {label} (skipped)");
                    continue;
                }
            }

            eprintln!("{progress_prefix}[{counter}] {label}");
            current_input = execute_step(step, &current_input, wf, file_config, &label).await?;
        }
        Ok((current_input, counter))
    })
}

/// Runs a single non-`switch` step (agent call, prompt call, or `jq`-only
/// data transform) and returns its output, with `jq` applied afterward if set.
async fn execute_step(
    step: &workflow::StepDefinition,
    current_input: &str,
    wf: &workflow::WorkflowFile,
    file_config: &ConfigFile,
    label: &str,
) -> Result<String> {
    if let Some(name_or_path) = &step.input_schema {
        let schema = schema::resolve_named_schema_value(&wf.json_schemas, name_or_path)
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

        let model_name = step
            .model
            .clone()
            .or_else(|| agent_file.model.clone())
            .or_else(|| wf.default.model.clone())
            .ok_or_else(|| {
                anyhow!(
                    "model is required for step '{label}'; set it on the step, its agent file, the workflow's default.model, or in {}",
                    config::CONFIG_FILE_NAME
                )
            })?;
        let reasoning_effort = step
            .reasoning_effort
            .or(agent_file.reasoning_effort)
            .or(wf.default.reasoning_effort);
        let settings = resolve_request_settings(
            model_name,
            reasoning_effort,
            None,
            None,
            &wf.models,
            file_config,
        )
        .with_context(|| format!("step '{label}'"))?;

        call_agent(&agent_file, &settings, &input, current_input)
            .await
            .with_context(|| format!("step '{label}'"))?
    } else if let Some(prompt_template) = &step.prompt {
        let model_name = step
            .model
            .clone()
            .or_else(|| wf.default.model.clone())
            .ok_or_else(|| {
                anyhow!(
                    "model is required for step '{label}'; set it on the step, the workflow's default.model, or in {}",
                    config::CONFIG_FILE_NAME
                )
            })?;
        let reasoning_effort = step.reasoning_effort.or(wf.default.reasoning_effort);
        let settings = resolve_request_settings(
            model_name,
            reasoning_effort,
            None,
            None,
            &wf.models,
            file_config,
        )
        .with_context(|| format!("step '{label}'"))?;

        let response_format = step
            .output_schema
            .as_deref()
            .map(|name_or_path| {
                let schema_name = step.schema_name.as_deref().unwrap_or("structured_output");
                match wf.json_schemas.get(name_or_path) {
                    Some(entry) => schema::build_response_format_from_entry(entry, schema_name),
                    None => {
                        schema::load_json_schema(std::path::Path::new(name_or_path), schema_name)
                    }
                }
            })
            .transpose()
            .with_context(|| format!("step '{label}'"))?;

        let prompt = workflow::render_prompt(prompt_template, current_input)
            .with_context(|| format!("step '{label}'"))?;

        let response = settings
            .complete(None, &prompt, response_format)
            .await
            .with_context(|| format!("step '{label}'"))?;

        response::render_response(&response, false, false)
            .with_context(|| format!("step '{label}'"))?
    } else {
        current_input.to_string()
    };

    if let Some(filter) = &step.jq {
        step_output = jq::apply(filter, &step_output).with_context(|| format!("step '{label}'"))?;
    }

    Ok(step_output)
}
