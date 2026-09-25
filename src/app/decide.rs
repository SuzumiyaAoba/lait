//! `lait decide`: one Jev-compatible decision request from the command line
//! — the same questions shape and client (`crate::jev`) a workflow
//! `decide:` step uses, for trying questions out before putting them in a
//! workflow. See docs/usage/ja/jev.md.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{
    async_io,
    chat::resolve_input_with_stdin_cancellable,
    cli::DecideArgs,
    config::{self, ConfigSource},
    engine::AppServices,
    jev,
};

pub(super) async fn run(
    args: DecideArgs,
    config_source: ConfigSource,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let file_config = super::load_config(&config_source, &cancellation).await?;

    let contents = async_io::read_to_string_cancellable(
        &args.questions,
        cancellation.clone(),
        async_io::MAX_READ_BYTES,
    )
    .await
    .with_context(|| {
        format!(
            "failed to read questions file '{}'",
            args.questions.display()
        )
    })?;
    let questions = serde_yaml::from_str(&contents)
        .map_err(anyhow::Error::new)
        .and_then(jev::Questions::from_yaml)
        .with_context(|| format!("invalid questions file '{}'", args.questions.display()))?;

    let Some(state) =
        resolve_input_with_stdin_cancellable(args.state.clone(), cancellation.clone()).await?
    else {
        bail!("no state given; pass STATE as an argument or pipe it via stdin");
    };
    let state = if args.json {
        serde_json::from_str(&state).context("--json: STATE is not valid JSON")?
    } else {
        Value::String(state)
    };

    let endpoint = config::resolve_jev_endpoint(args.base_url, args.api_key, &file_config)?;
    let model = jev::resolve_model(args.model.as_deref(), &file_config);
    let services = Arc::new(AppServices::new(Arc::clone(&file_config)));
    let decision = services
        .clone()
        .finish(jev::decide(
            &services,
            &endpoint,
            &model,
            &questions,
            jev::state_from_value(state),
            cancellation,
        ))
        .await?;

    let output = if args.full {
        decision.raw
    } else {
        Value::Object(decision.answers)
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
