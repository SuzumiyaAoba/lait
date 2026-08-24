use std::path::Path;

use anyhow::{Context, Result, anyhow};

use crate::{
    cli::{ChatArgs, Cli, Command, RunArgs},
    config, jq, llm, response, schema, workflow,
};

const DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";

pub(crate) async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Run(run_args)) => run_workflow(run_args, cli.no_config).await,
        None => run_chat(cli.chat, cli.no_config).await,
    }
}

async fn run_chat(chat: ChatArgs, no_config: bool) -> Result<()> {
    let prompt = chat.prompt.clone().ok_or_else(|| {
        anyhow!("a PROMPT is required; provide one, or use `lait run <FILE> <PROMPT>`")
    })?;

    let file_config = config::load_config(no_config)?;
    let model_name = chat
        .model
        .or_else(|| file_config.default.model.clone())
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "model is required; provide --model, set LLM_MODEL, or specify default.model in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
    let resolved_model = config::resolve_model(model_name, &file_config)?;
    let base_url = chat
        .base_url
        .or(resolved_model.base_url)
        .or(file_config.base_url)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let base_url = base_url.trim_end_matches('/');
    if base_url.is_empty() {
        return Err(anyhow!("base URL must not be empty"));
    }

    let api_key = chat
        .api_key
        .or(resolved_model.api_key)
        .or(file_config.api_key)
        .unwrap_or_else(|| {
            // async-openai always builds an Authorization header from its config.
            // LM Studio ignores the value, so use a non-empty dummy key when no
            // key was supplied instead of making local requests fail on an empty
            // header value.
            "lm-studio".to_owned()
        });

    let response_format = chat
        .json_schema
        .as_deref()
        .map(|path| schema::load_json_schema(path, &chat.schema_name))
        .transpose()?;

    let reasoning_effort = chat
        .reasoning_effort
        .or(resolved_model.reasoning_effort)
        .or(file_config.default.reasoning_effort);

    let response = llm::complete(llm::CompletionRequest {
        base_url,
        api_key: &api_key,
        model_id: &resolved_model.model_id,
        reasoning_effort,
        response_format,
        prompt: &prompt,
    })
    .await?;

    let output = response::render_response(&response, chat.json, chat.show_reasoning)?;
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

        let mut step_output = if let Some(prompt_template) = &step.prompt {
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
            let resolved_model = match config::resolve_model_alias(&model_name, &wf.models)? {
                Some(resolved) => resolved,
                None => config::resolve_model(model_name, &file_config)?,
            };
            let base_url = resolved_model
                .base_url
                .or_else(|| file_config.base_url.clone())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
            let base_url = base_url.trim_end_matches('/');
            if base_url.is_empty() {
                return Err(anyhow!("base URL must not be empty"));
            }
            let api_key = resolved_model
                .api_key
                .or_else(|| file_config.api_key.clone())
                .unwrap_or_else(|| "lm-studio".to_owned());
            let reasoning_effort = step
                .reasoning_effort
                .or(wf.default.reasoning_effort)
                .or(resolved_model.reasoning_effort)
                .or(file_config.default.reasoning_effort);
            let response_format = step
                .json_schema
                .as_deref()
                .map(|name_or_path| {
                    let schema_name = step.schema_name.as_deref().unwrap_or("structured_output");
                    match wf.json_schemas.get(name_or_path) {
                        Some(workflow::JsonSchemaEntry::Inline { schema }) => {
                            schema::build_json_schema(schema.clone(), schema_name)
                        }
                        Some(workflow::JsonSchemaEntry::FilePath { file_path }) => {
                            schema::load_json_schema(file_path, schema_name)
                        }
                        None => schema::load_json_schema(Path::new(name_or_path), schema_name),
                    }
                })
                .transpose()
                .with_context(|| format!("step '{label}'"))?;

            let prompt = workflow::render_prompt(prompt_template, &current_input)
                .with_context(|| format!("step '{label}'"))?;

            let response = llm::complete(llm::CompletionRequest {
                base_url,
                api_key: &api_key,
                model_id: &resolved_model.model_id,
                reasoning_effort,
                response_format,
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
