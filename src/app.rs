use anyhow::{Context, Result, anyhow};

use crate::{
    agent,
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

    let response = llm::complete(llm::CompletionRequest {
        base_url: &settings.base_url,
        api_key: &settings.api_key,
        model_id: &settings.resolved_model.model_id,
        reasoning_effort: settings.reasoning_effort,
        response_format,
        system_prompt: None,
        prompt: &prompt,
    })
    .await?;

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

    let system_prompt = template::render(&agent_file.system_prompt_template, &input)
        .with_context(|| format!("agent '{}'", args.file.display()))?;
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
        .transpose()
        .with_context(|| format!("agent '{}'", args.file.display()))?;

    let response = llm::complete(llm::CompletionRequest {
        base_url: &settings.base_url,
        api_key: &settings.api_key,
        model_id: &settings.resolved_model.model_id,
        reasoning_effort: settings.reasoning_effort,
        response_format,
        system_prompt: Some(&system_prompt),
        prompt: &args.input,
    })
    .await?;

    let output = response::render_response(&response, false, false)?;
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

    let total = wf.steps.len();
    let mut current_input = run_args.prompt;
    for (index, step) in wf.steps.iter().enumerate() {
        let label = step
            .id
            .clone()
            .unwrap_or_else(|| format!("step-{}", index + 1));
        eprintln!("[{}/{total}] {label}", index + 1);

        let mut step_output = if let Some(agent_path) = &step.agent {
            let agent_file =
                agent::load_agent(agent_path).with_context(|| format!("step '{label}'"))?;

            let input = template::parse_input(&current_input);
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
                &file_config,
            )
            .with_context(|| format!("step '{label}'"))?;

            let system_prompt = template::render(&agent_file.system_prompt_template, &input)
                .with_context(|| format!("step '{label}'"))?;
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
                .transpose()
                .with_context(|| format!("step '{label}'"))?;

            let response = llm::complete(llm::CompletionRequest {
                base_url: &settings.base_url,
                api_key: &settings.api_key,
                model_id: &settings.resolved_model.model_id,
                reasoning_effort: settings.reasoning_effort,
                response_format,
                system_prompt: Some(&system_prompt),
                prompt: &current_input,
            })
            .await
            .with_context(|| format!("step '{label}'"))?;

            response::render_response(&response, false, false)
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
                &file_config,
            )
            .with_context(|| format!("step '{label}'"))?;

            let response_format = step
                .json_schema
                .as_deref()
                .map(|name_or_path| {
                    let schema_name = step.schema_name.as_deref().unwrap_or("structured_output");
                    match wf.json_schemas.get(name_or_path) {
                        Some(entry) => schema::build_response_format_from_entry(entry, schema_name),
                        None => schema::load_json_schema(
                            std::path::Path::new(name_or_path),
                            schema_name,
                        ),
                    }
                })
                .transpose()
                .with_context(|| format!("step '{label}'"))?;

            let prompt = workflow::render_prompt(prompt_template, &current_input)
                .with_context(|| format!("step '{label}'"))?;

            let response = llm::complete(llm::CompletionRequest {
                base_url: &settings.base_url,
                api_key: &settings.api_key,
                model_id: &settings.resolved_model.model_id,
                reasoning_effort: settings.reasoning_effort,
                response_format,
                system_prompt: None,
                prompt: &prompt,
            })
            .await
            .with_context(|| format!("step '{label}'"))?;

            response::render_response(&response, false, false)
                .with_context(|| format!("step '{label}'"))?
        } else {
            current_input.clone()
        };

        if let Some(filter) = &step.jq {
            step_output =
                jq::apply(filter, &step_output).with_context(|| format!("step '{label}'"))?;
        }

        current_input = step_output;
    }

    println!("{current_input}");
    Ok(())
}
