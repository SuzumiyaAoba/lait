//! `lait prompt run`/`lait agent run`: rendering a named prompt or agent
//! file's own template/schema/completion pipeline and sending the result as
//! a plain, tool-free (prompt) or tool-capable (agent) request. Split out of
//! `app.rs` to keep that module to pure dispatch (see its own doc comment)
//! — `run`'s `AsyncCommand::PromptRun`/`AsyncCommand::AgentRun` arms are this
//! module's only callers.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

use crate::{
    agent, chat,
    cli::{AgentRunArgs, OutputArgs, PromptRunArgs, ReportingArgs},
    config::{self, ConfigSource, ModelMap},
    engine::{
        AgentTurn, CapabilityOverrides, EndpointOverrides, PromptTurn, SamplingOverrides,
        agent_file_settings, call_agent, resolve_request_settings,
    },
    prompt, report, response, template, usage,
    workflow::{self, exec::announce_named_file},
};

use super::build_run_context;

/// The reporting-related pieces `finish_prompt_or_agent_run` needs beyond
/// the run's own content, bundled into one parameter (like `ProcessTasks`
/// in `process.rs`) so adding them alongside `kind`/`output`/`model_id`/
/// `prompt` doesn't trip `clippy::too_many_arguments`.
struct RunReport<'a> {
    output_args: &'a OutputArgs,
    reporting: &'a ReportingArgs,
    file_config: &'a config::ConfigFile,
    usage: &'a usage::UsageTally,
}

/// Emits and records a `run_prompt`/`run_agent` result: prints `output`
/// (respecting `--output`/`-o`), then records the run to history and prints
/// the usage summary when asked. The two callers differ only in `kind`
/// (`"prompt"`/`"agent"`), which model id to attribute the run to, and which
/// text counts as the "prompt" in history — everything else here used to be
/// copied verbatim between them.
fn finish_prompt_or_agent_run(
    kind: &'static str,
    output: &str,
    model_id: &str,
    prompt: &str,
    report: RunReport<'_>,
) -> Result<()> {
    report::emit_run_output(
        output,
        report.usage.total(),
        report.output_args,
        report.file_config,
    )?;
    report::finish_run(
        report::RunRecord {
            kind,
            model: Some(model_id),
            prompt,
            response: output,
        },
        report.reporting.no_history,
        report.file_config,
        report.usage,
        report.reporting.show_usage,
    )
}

/// Shared by [`run_prompt`]/[`run_agent`]: both resolve `INPUT` the same way
/// (`chat::resolve_input_with_stdin_cancellable`) and fail identically when
/// neither a positional argument nor piped stdin supplied one.
fn missing_input_error() -> anyhow::Error {
    anyhow!("an INPUT is required; provide one or pipe input via stdin")
}

/// Runs `lait prompt run <NAME> [INPUT]` (`lait prompt list` is handled
/// separately, synchronously, by `prompt::list` — see `needs_async_runtime`/
/// `run_blocking`): renders the named prompt (see `prompt::render_named`)
/// and sends the result as a plain, tool-free, non-streamed request. This
/// subcommand form is intentionally narrower than `-p`/`--prompt-name` on
/// the main chat invocation (no `--model`/`--stream`/`--mcp`/... overrides —
/// see `docs/usage/ja/prompts.md`); reach for `-p` when those are needed.
/// `-o`/`--render`/`--json`/`--show-usage`/`--no-history` work the same as
/// every other `run_*` entry point (see `cli::OutputArgs`).
pub(super) async fn run_prompt(
    args: PromptRunArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    let file_config =
        Arc::new(config::load_config_cancellable(&config_source, Some(cancel.clone())).await?);

    let raw_input =
        chat::resolve_input_with_stdin_cancellable(args.input.clone(), Some(cancel.clone()))
            .await?
            .ok_or_else(missing_input_error)?;
    let (prompt_text, prompt_model) =
        prompt::render_named(&args.name, &raw_input, &args.var.var, &file_config)?;

    // `.filter` folds a blank-but-present `model:` into the same "none
    // configured" branch below, so it gets this prompt-specific hint
    // ('prompts.<name>.model'/'default.model') instead of
    // `config::resolve_model`'s generic "model name must not be empty" —
    // `resolve_request_settings` still enforces the latter as a backstop for
    // any model name reaching it by another path, but this is the more
    // useful message for the one a `lait prompt` author would actually see.
    let model_name = prompt_model
        .or_else(|| file_config.default.model.clone())
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "model is required for prompt '{}'; set 'prompts.{}.model' or default.model in {}",
                args.name,
                args.name,
                config::CONFIG_FILE_NAME
            )
        })?;
    let settings = resolve_request_settings(
        model_name,
        SamplingOverrides::default(),
        EndpointOverrides::default(),
        CapabilityOverrides::default(),
        &ModelMap::default(),
        &file_config,
    )?
    .with_usage_label(format!("prompt '{}'", args.name));

    let (services, env) = build_run_context(&file_config, cache_override, approve_tools, cancel);
    let response = services
        .finish(settings.complete(
            &env,
            &[],
            PromptTurn::simple(None, &prompt_text),
            None,
            Some(env.operation_token()),
        ))
        .await?;
    let output = response::render_plain(&response)?;
    finish_prompt_or_agent_run(
        "prompt",
        &output,
        &settings.resolved_model.model_id,
        &prompt_text,
        RunReport {
            output_args: &args.output,
            reporting: &args.reporting,
            file_config: &file_config,
            usage: &env.usage,
        },
    )
}

pub(super) async fn run_agent(
    args: AgentRunArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    // These four reads are independent of each other: stdin/argument input
    // resolution only needs `args.input`, loading and canonicalizing the
    // agent file only need `args.file`, and config loading only needs
    // `config_source`. Running them concurrently rather than one after
    // another shortens the wall-clock delay before the agent actually
    // starts.
    //
    // Behavior note: when two of these fail at once, which error surfaces
    // is now whichever `try_join!` polls to a `Result::Err` first rather
    // than a fixed left-to-right order (stdin/input, then agent file, then
    // config) — none of these four reads name each other in their error
    // text, so there is no risk of a confusing partial message, only a
    // different tie-break among independent failures.
    let (raw_input, agent_file, canonical_agent_path, config) = tokio::try_join!(
        chat::resolve_input_with_stdin_cancellable(args.input.clone(), Some(cancel.clone())),
        agent::load_agent_cancellable(&args.file, Some(cancel.clone())),
        async {
            crate::async_io::canonicalize(&args.file, Some(cancel.clone()))
                .await
                .with_context(|| {
                    format!(
                        "failed to resolve agent file path '{}'",
                        args.file.display()
                    )
                })
        },
        config::load_config_cancellable(&config_source, Some(cancel.clone())),
    )?;
    let raw_input = raw_input.ok_or_else(missing_input_error)?;
    let file_config = Arc::new(config);

    announce_named_file(
        "==>",
        agent_file.name.as_deref(),
        agent_file.description.as_deref(),
    );

    let input = template::parse_input(&raw_input);
    agent_file
        .validate_input(&input)
        .with_context(|| format!("agent '{}'", args.file.display()))?;

    let usage_label = agent_file
        .name
        .clone()
        .unwrap_or_else(|| args.file.display().to_string());
    let settings =
        agent_file_settings(&agent_file, &file_config, None)?.with_usage_label(usage_label);

    let (services, env) = build_run_context(&file_config, cache_override, approve_tools, cancel);
    let output = services
        .finish(call_agent(
            &agent_file,
            &settings,
            &env,
            AgentTurn::simple(&input, &raw_input),
            &workflow::StepOutputs::new(),
            std::slice::from_ref(&canonical_agent_path),
            Some(env.operation_token()),
        ))
        .await
        .with_context(|| format!("agent '{}'", args.file.display()))?;
    finish_prompt_or_agent_run(
        "agent",
        &output,
        &settings.resolved_model.model_id,
        &raw_input,
        RunReport {
            output_args: &args.output,
            reporting: &args.reporting,
            file_config: &file_config,
            usage: &env.usage,
        },
    )
}
