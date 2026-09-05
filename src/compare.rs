//! The `lait compare` subcommand: sends one prompt to two or more models
//! concurrently and reports each one's response, timing, and usage side by
//! side. See docs/usage/ja/compare.md.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use futures_util::future::join_all;
use serde::Serialize;

use crate::{
    app,
    cli::CompareArgs,
    config::{self, ConfigSource, ModelMap},
    engine::{
        AppContext, CapabilityOverrides, PromptTurn, SamplingOverrides, resolve_request_settings,
    },
    response, signal,
};

/// One model's outcome, serialized as-is for `--json` (an array of these).
#[derive(Debug, Serialize)]
struct ModelResult {
    model: String,
    model_id: String,
    duration_ms: u64,
    usage: Option<response::Usage>,
    content: Option<String>,
    error: Option<String>,
}

pub(crate) async fn run(
    args: CompareArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    if args.models.len() < 2 {
        bail!("`lait compare` requires at least two `--model` values");
    }

    signal::spawn_handler(cancel.clone());
    let file_config = Arc::new(config::load_config(&config_source)?);

    let prompt = app::resolve_input_with_stdin(args.prompt.clone())?
        .ok_or_else(|| anyhow!("a PROMPT is required; provide one or pipe input via stdin"))?;

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
            None,
            None,
            CapabilityOverrides::default(),
            &ModelMap::default(),
            &file_config,
        )?
        .with_usage_label(model_name.clone());
        settings_list.push((model_name.clone(), settings));
    }

    let (cache_enabled, cache_ttl) = app::resolve_cache_settings(cache_override, &file_config);
    let env = AppContext::new(Arc::clone(&file_config))
        .with_cancel(cancel)
        .with_cache(cache_enabled, cache_ttl);

    let futures = settings_list.iter().map(|(model_name, settings)| {
        let prompt = &prompt;
        let env = &env;
        async move {
            let turn = PromptTurn::simple(None, prompt);
            let started = Instant::now();
            let outcome = settings
                .complete(env, &[], turn, None, env.cancel.clone())
                .await;
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            match outcome {
                Ok(response) => ModelResult {
                    model: model_name.clone(),
                    model_id: settings.resolved_model.model_id.clone(),
                    duration_ms,
                    usage: response.usage,
                    content: Some(response::content_text(&response).to_owned()),
                    error: None,
                },
                Err(error) => ModelResult {
                    model: model_name.clone(),
                    model_id: settings.resolved_model.model_id.clone(),
                    duration_ms,
                    usage: None,
                    content: None,
                    error: Some(format!("{error:#}")),
                },
            }
        }
    });
    let results = env.finish(join_all(futures)).await;

    let any_error = results.iter().any(|result| result.error.is_some());

    if args.json {
        println!("{}", serde_json::to_string(&results)?);
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
                    println!("usage: {usage}");
                }
                if let Some(content) = &result.content {
                    println!("{content}");
                }
            }
        }
    }
}
