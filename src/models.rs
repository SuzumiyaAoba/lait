//! The `lait models` subcommand: lists the model aliases configured in
//! `lait.config.yml` (with the `default.model` marked), or — with
//! `--remote` — asks the server itself for its available models via
//! `GET /v1/models`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    cli::ModelsArgs,
    config::{self, ConfigFile, ResolvedModel},
    llm,
};

pub(crate) async fn run(args: ModelsArgs, no_config: bool) -> Result<()> {
    let file_config = config::load_config(no_config)?;
    if args.remote {
        list_remote(&args, &file_config).await
    } else {
        list_local(&args, &file_config)
    }
}

/// The `--remote`-less path, callable without an async runtime — see
/// `app::run_blocking`.
pub(crate) fn run_local(args: ModelsArgs, no_config: bool) -> Result<()> {
    let file_config = config::load_config(no_config)?;
    list_local(&args, &file_config)
}

/// One `models:` alias row, resolved through the same
/// `config::resolve_model_alias` a request uses, so the listing can never
/// disagree with what actually runs.
struct AliasRow<'a> {
    name: &'a str,
    is_default: bool,
    resolved: ResolvedModel,
    /// How many further definitions the alias lists beyond the first (only
    /// the first ever takes effect — see `config::resolve_model_alias`).
    extra_definitions: usize,
}

/// Collects the configured aliases sorted by name (the underlying map has no
/// stable order), marking the one `default.model` names. An alias
/// `resolve_model_alias` rejects (an empty definition list, an empty
/// model_id) is skipped — running it would fail anyway, and a listing
/// should still show the valid rest.
fn alias_rows(file_config: &ConfigFile) -> Vec<AliasRow<'_>> {
    let default_model = file_config.default.model.as_deref();
    let mut rows: Vec<AliasRow<'_>> = file_config
        .models
        .iter()
        .filter_map(|(name, definitions)| {
            let resolved = config::resolve_model_alias(name, &file_config.models)
                .ok()
                .flatten()?;
            Some(AliasRow {
                name,
                is_default: Some(name.as_str()) == default_model,
                resolved,
                extra_definitions: definitions.len().saturating_sub(1),
            })
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(b.name));
    rows
}

/// The DEFAULTS column: the per-model default parameters that are actually
/// set, compactly (`reasoning=high temperature=0.7 ...`), or `-` when none
/// are.
fn defaults_column(resolved: &ResolvedModel) -> String {
    let mut parts = Vec::new();
    if let Some(effort) = resolved.reasoning_effort {
        parts.push(format!("reasoning={}", effort.as_str()));
    }
    if let Some(temperature) = resolved.temperature {
        parts.push(format!("temperature={temperature}"));
    }
    if let Some(top_p) = resolved.top_p {
        parts.push(format!("top_p={top_p}"));
    }
    if let Some(max_tokens) = resolved.max_tokens {
        parts.push(format!("max_tokens={max_tokens}"));
    }
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join(" ")
    }
}

fn list_local(args: &ModelsArgs, file_config: &ConfigFile) -> Result<()> {
    let rows = alias_rows(file_config);
    let default_model = file_config.default.model.as_deref();
    // `default.model` may also name a raw model id rather than an alias;
    // worth saying so instead of leaving the default seemingly unset.
    let default_is_alias = rows.iter().any(|row| row.is_default);

    if args.json {
        let models: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "name": row.name,
                    "default": row.is_default,
                    "model_id": row.resolved.model_id,
                    "base_url": row.resolved.base_url,
                    "api_key_set": row.resolved.api_key.is_some(),
                    "reasoning_effort": row.resolved.reasoning_effort.map(|e| e.as_str()),
                    "temperature": row.resolved.temperature,
                    "top_p": row.resolved.top_p,
                    "max_tokens": row.resolved.max_tokens,
                    "extra_definitions": row.extra_definitions,
                })
            })
            .collect();
        let output = serde_json::json!({
            "default_model": default_model,
            "models": models,
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "no model aliases defined in {}; use `lait models --remote` to list the server's models",
            config::CONFIG_FILE_NAME
        );
        if let Some(name) = default_model {
            println!("default model: {name} (used as a raw model id)");
        }
        return Ok(());
    }

    // `base_url` may hold an unexpanded `${VAR}` placeholder; it is shown
    // as written — expanding it here would print a secret-adjacent value.
    let mut table = vec![[
        "NAME".to_owned(),
        "MODEL_ID".to_owned(),
        "BASE_URL".to_owned(),
        "DEFAULTS".to_owned(),
    ]];
    for row in &rows {
        let mut name = String::new();
        if row.is_default {
            name.push('*');
        }
        name.push_str(row.name);
        if row.extra_definitions > 0 {
            name.push_str(&format!(" (+{} unused)", row.extra_definitions));
        }
        table.push([
            name,
            row.resolved.model_id.clone(),
            row.resolved
                .base_url
                .clone()
                .unwrap_or_else(|| "-".to_owned()),
            defaults_column(&row.resolved),
        ]);
    }

    let mut widths = [0usize; 4];
    for row in &table {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for row in &table {
        let line = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>()
            .join("  ");
        println!("{}", line.trim_end());
    }
    if default_is_alias {
        println!("(* = default.model)");
    } else if let Some(name) = default_model {
        println!("default model: {name} (used as a raw model id)");
    }
    Ok(())
}

/// The subset of a `GET /v1/models` response lait reads: the model ids.
#[derive(Deserialize)]
struct RemoteModelsResponse {
    #[serde(default)]
    data: Vec<RemoteModel>,
}

#[derive(Deserialize)]
struct RemoteModel {
    id: String,
}

async fn list_remote(args: &ModelsArgs, file_config: &ConfigFile) -> Result<()> {
    // The same endpoint resolution as a completion request (CLI/env >
    // config), except model aliases play no part: `--remote` asks one
    // concrete server, and no API key means no Authorization header rather
    // than the completion path's dummy key.
    let (base_url, api_key) = crate::app::resolve_endpoint(
        args.base_url.clone(),
        args.api_key.clone(),
        None,
        None,
        file_config,
    )?;

    let url = format!("{base_url}/models");
    let mut request = llm::http_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(30));
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to request {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("failed to read the response from {url}"))?;
    if !status.is_success() {
        bail!("GET {url} failed with {status}: {}", body.trim());
    }

    if args.json {
        // Machine-readable means the server's own answer, verbatim.
        println!("{}", body.trim_end());
        return Ok(());
    }

    let parsed: RemoteModelsResponse = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse the response from {url} as a model list"))?;
    if parsed.data.is_empty() {
        println!("(the server reported no models)");
        return Ok(());
    }
    for model in parsed.data {
        println!("{}", model.id);
    }
    Ok(())
}
