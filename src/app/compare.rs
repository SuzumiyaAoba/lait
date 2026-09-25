//! The `lait compare` subcommand: sends one prompt to two or more models
//! concurrently and reports each one's response, timing, and usage side by
//! side. See docs/usage/ja/compare.md.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use serde::Serialize;

use crate::{
    async_io, chat,
    cli::CompareArgs,
    config::{ConfigSource, DefaultSettings, ModelMap},
    engine::{
        CapabilityOverrides, EndpointOverrides, PromptTurn, SamplingOverrides,
        resolve_request_settings,
    },
    error::missing_prompt_error,
    response,
};

/// One model's outcome, serialized as-is for `--json` (an array of these).
#[derive(Debug, Serialize)]
struct ModelResult {
    model: String,
    model_id: String,
    duration_ms: u64,
    usage: Option<response::Usage>,
    /// Estimated USD cost of `usage` at this model's `pricing:` rates —
    /// `None` when either `usage` itself is `None` or the resolved model has
    /// no `pricing:` configured (never `Some(0.0)` for "unpriced"; see
    /// `config::Pricing`'s doc comment).
    cost_usd: Option<f64>,
    content: Option<String>,
    error: Option<String>,
}

pub(super) async fn run(
    args: CompareArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    if args.models.len() < 2 {
        bail!("`lait compare` requires at least two `--model` values");
    }

    let file_config = super::load_config(&config_source, &cancel).await?;

    let prompt = chat::resolve_input_with_stdin_cancellable(args.prompt.clone(), cancel.clone())
        .await?
        .ok_or_else(missing_prompt_error)?;
    let system_prompt = resolve_system_prompt(&args, &file_config.default, cancel.clone()).await?;
    let capabilities = CapabilityOverrides {
        mcp: (!args.mcp.is_empty()).then(|| args.mcp.clone()),
        subagents: (!args.subagent.is_empty()).then(|| args.subagent.clone()),
        tools: (!args.tool.is_empty()).then(|| args.tool.clone()),
        ..CapabilityOverrides::default()
    };

    let sampling = SamplingOverrides {
        reasoning_effort: args.reasoning_effort,
        temperature: args.temperature,
        top_p: args.top_p,
        max_tokens: args.max_tokens,
    };

    let mut settings_list = Vec::with_capacity(args.models.len());
    for model_name in &args.models {
        let settings = resolve_request_settings(
            model_name.clone(),
            sampling,
            EndpointOverrides::default(),
            capabilities.clone(),
            &ModelMap::default(),
            &file_config,
        )?
        .with_usage_label(model_name.clone());
        settings_list.push((model_name.clone(), settings));
    }

    let (services, env) = super::build_run_context(&file_config, cache_override, false, cancel);

    let futures = settings_list.iter().map(|(model_name, settings)| {
        let prompt = &prompt;
        let system_prompt = system_prompt.as_deref();
        let env = &env;
        async move {
            let turn = PromptTurn::simple(system_prompt, prompt);
            let started = Instant::now();
            let outcome = settings
                .complete(env, &[], turn, None, env.operation_token())
                .await;
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            match outcome {
                Ok(response) => ModelResult {
                    model: model_name.clone(),
                    model_id: settings.resolved_model.model_id.clone(),
                    duration_ms,
                    usage: response.usage,
                    cost_usd: response
                        .usage
                        .zip(settings.resolved_model.pricing)
                        .map(|(usage, pricing)| pricing.cost(usage)),
                    content: Some(response::content_text(&response).to_owned()),
                    error: None,
                },
                Err(error) => ModelResult {
                    model: model_name.clone(),
                    model_id: settings.resolved_model.model_id.clone(),
                    duration_ms,
                    usage: None,
                    cost_usd: None,
                    content: None,
                    error: Some(format!("{error:#}")),
                },
            }
        }
    });
    let results = services.finish(join_all(futures)).await;

    let any_error = results.iter().any(|result| result.error.is_some());

    if args.json {
        println!("{}", serde_json::to_string(&results)?);
    } else if args.markdown {
        print!("{}", markdown_report(&results));
    } else {
        print_report(&results);
    }

    if any_error {
        bail!(
            "{} of {} model(s) failed",
            results
                .iter()
                .filter(|result| result.error.is_some())
                .count(),
            results.len()
        );
    }
    Ok(())
}

/// `--system`, else `--system-file`'s contents, else `default.system`.
async fn resolve_system_prompt(
    args: &CompareArgs,
    defaults: &DefaultSettings,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<Option<String>> {
    if let Some(text) = &args.system {
        return Ok(Some(text.clone()));
    }
    if let Some(path) = &args.system_file {
        let text =
            async_io::read_to_string_cancellable(path, cancellation, async_io::MAX_READ_BYTES)
                .await
                .with_context(|| {
                    format!("failed to read system prompt file '{}'", path.display())
                })?;
        return Ok(Some(text.trim_end().to_owned()));
    }
    Ok(defaults.system.clone())
}

/// The `--markdown` report: a summary table, then every model's response
/// (or error) under its own heading. Table cells escape `|` so a model name
/// or error message can't break the table.
fn markdown_report(results: &[ModelResult]) -> String {
    let cell = |text: &str| text.replace('|', "\\|").replace('\n', " ");
    let mut out = String::from(
        "| model | model_id | time | prompt | completion | total | cost | status |\n\
         | --- | --- | ---: | ---: | ---: | ---: | ---: | --- |\n",
    );
    for result in results {
        let (prompt, completion, total) = match result.usage {
            Some(usage) => (
                usage.prompt_tokens.to_string(),
                usage.completion_tokens.to_string(),
                usage.total_tokens.to_string(),
            ),
            None => ("-".to_owned(), "-".to_owned(), "-".to_owned()),
        };
        let cost = result
            .cost_usd
            .map(crate::usage::format_cost)
            .unwrap_or_else(|| "-".to_owned());
        let status = match &result.error {
            Some(error) => format!("error: {}", cell(error)),
            None => "ok".to_owned(),
        };
        out.push_str(&format!(
            "| {} | {} | {}ms | {prompt} | {completion} | {total} | {cost} | {status} |\n",
            cell(&result.model),
            cell(&result.model_id),
            result.duration_ms,
        ));
    }
    for result in results {
        out.push_str(&format!("\n## {} ({})\n\n", result.model, result.model_id));
        match (&result.error, &result.content) {
            (Some(error), _) => out.push_str(&format!("> error: {error}\n")),
            (None, Some(content)) => {
                out.push_str(content);
                out.push('\n');
            }
            (None, None) => {}
        }
    }
    out
}

fn print_report(results: &[ModelResult]) {
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!("=== {} ({}) ===", result.model, result.model_id);
        println!("time: {}ms", result.duration_ms);
        match &result.error {
            Some(error) => println!("error: {error}"),
            None => {
                if let Some(usage) = result.usage {
                    match result.cost_usd {
                        Some(cost) => {
                            println!("usage: {usage} ({})", crate::usage::format_cost(cost))
                        }
                        None => println!("usage: {usage}"),
                    }
                }
                if let Some(content) = &result.content {
                    println!("{content}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelResult, markdown_report};

    fn result(model: &str, error: Option<&str>) -> ModelResult {
        ModelResult {
            model: model.to_owned(),
            model_id: format!("{model}-id"),
            duration_ms: 12,
            usage: error.is_none().then_some(crate::response::Usage {
                prompt_tokens: 3,
                completion_tokens: 4,
                total_tokens: 7,
            }),
            cost_usd: None,
            content: error.is_none().then(|| format!("answer from {model}")),
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn markdown_report_has_a_summary_table_and_a_section_per_model() {
        let report = markdown_report(&[result("a", None), result("b", Some("boom | bad"))]);
        assert!(
            report.contains("| a | a-id | 12ms | 3 | 4 | 7 | - | ok |"),
            "{report}"
        );
        assert!(
            report.contains("| b | b-id | 12ms | - | - | - | - | error: boom \\| bad |"),
            "{report}"
        );
        assert!(
            report.contains("\n## a (a-id)\n\nanswer from a\n"),
            "{report}"
        );
        assert!(
            report.contains("\n## b (b-id)\n\n> error: boom | bad\n"),
            "{report}"
        );
    }
}
