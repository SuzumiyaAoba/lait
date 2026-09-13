//! Chat-turn building blocks shared by `app::run_chat` (single-shot), `repl`
//! (the interactive REPL), and `compare` (multi-model side-by-side): stdin
//! resolution, model/system-prompt/cache settings, session history, and
//! turn recording. Pulled out of `app` because `repl`/`compare` calling back
//! into `app` for these was the wrong direction — `app` only dispatches to
//! them, it doesn't need anything they'd call back for.

use anyhow::{Context, Result, anyhow};
use async_openai::types::chat::ChatCompletionRequestMessage;

use crate::{
    async_io,
    cli::SharedChatArgs,
    config::{self, ConfigFile, ModelMap},
    engine::{
        CapabilityOverrides, EndpointOverrides, RequestSettings, SamplingOverrides,
        resolve_request_settings,
    },
    report, response, session,
};

/// Reads all of stdin into a string, trimming trailing newlines (piped text
/// almost always ends in one, and a prompt should not).
fn read_stdin_text() -> Result<String> {
    use std::io::Read;

    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .context("failed to read from stdin")?;
    Ok(buffer.trim_end_matches(['\n', '\r']).to_owned())
}

/// Reads a positional prompt/input and optional piped stdin for async
/// entry points. Reading stdin is kept on the bounded blocking-I/O worker so a
/// FIFO or a pipe with no EOF cannot hold the Tokio runtime past Ctrl-C.
pub(crate) async fn resolve_input_with_stdin_cancellable(
    positional: Option<String>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Option<String>> {
    use std::io::IsTerminal;

    let read_stdin = positional.as_deref() == Some("-") || !std::io::stdin().is_terminal();
    let piped_text = if read_stdin {
        Some(crate::async_io::run_blocking(move |_| read_stdin_text(), cancellation).await?)
            .filter(|text| !text.trim().is_empty())
    } else {
        None
    };
    Ok(match (positional, piped_text) {
        (Some(argument), piped) if argument == "-" => piped,
        (Some(argument), Some(piped)) => Some(format!("{argument}\n\n{piped}")),
        (Some(argument), None) => Some(argument),
        (None, piped) => piped,
    })
}

/// Records one finished chat turn: appends it to `--session`'s log (when
/// set) and to `lait history` (unless suppressed) — the shared tail of
/// `run_chat`'s streamed and non-streamed paths and `repl::run`'s per-turn
/// loop.
pub(crate) fn finish_chat_turn(
    session_name: Option<&str>,
    no_history: bool,
    file_config: &ConfigFile,
    model_id: &str,
    prompt: &str,
    response: &str,
    usage: Option<response::Usage>,
) -> Result<()> {
    if let Some(name) = session_name {
        session::append_turn(name, prompt, response)?;
    }
    report::record_history(
        no_history,
        file_config,
        "chat",
        Some(model_id),
        prompt,
        response,
        usage,
    )
}

/// Resolves whether the response disk cache is enabled for this invocation,
/// and its TTL: `cache_override` (from `--cache`/`--no-cache`) wins when set,
/// else `default.cache` in lait.config.yml, else off. `default.cache_ttl`
/// applies regardless of which layer enabled the cache. Every async command
/// handler in `app` calls this once, right before building its `RunContext`,
/// with the same `cache_override` `app::run` resolved up front — see
/// `RunContext::with_cache`.
pub(crate) fn resolve_cache_settings(
    cache_override: Option<bool>,
    file_config: &ConfigFile,
) -> (bool, Option<u64>) {
    let enabled = cache_override.unwrap_or(file_config.default.cache.unwrap_or(false));
    (enabled, file_config.default.cache_ttl)
}

/// Resolves chat mode's system prompt: `--system` text, else `--system-file`
/// contents, else `default.system` from lait.config.yml (`--system` and
/// `--system-file` conflict at the clap level, so their order here never
/// actually decides anything). Reads `--system-file` through
/// `async_io::read_to_string_cancellable` rather than a plain synchronous
/// read: this runs once at the start of `run_chat`/`repl::run`, both already
/// holding the invocation's cancellation token at that point, and a blocked
/// read here would otherwise delay Ctrl-C the same way an un-migrated cache/
/// cassette read would (see `cache::load`'s doc comment for the same
/// reasoning).
pub(crate) async fn resolve_system_prompt(
    shared: &SharedChatArgs,
    file_config: &ConfigFile,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Option<String>> {
    if let Some(text) = &shared.system {
        return Ok(Some(text.clone()));
    }
    if let Some(path) = &shared.system_file {
        let text =
            async_io::read_to_string_cancellable(path, cancellation, async_io::MAX_READ_BYTES)
                .await
                .with_context(|| {
                    format!("failed to read system prompt file '{}'", path.display())
                })?;
        return Ok(Some(text.trim_end().to_owned()));
    }
    Ok(file_config.default.system.clone())
}

/// Resolves a chat turn's `RequestSettings` from `shared` (the options common
/// to single-shot chat and `lait chat`'s REPL — see `SharedChatArgs`) and
/// `file_config`. Shared by `run_chat` and `repl::run`, which both need
/// exactly this: chat's own model-resolution rule (`--model`/`LLM_MODEL` >
/// `prompt_model_fallback` > `default.model`) plus the sampling/capability
/// overrides every chat turn carries. The REPL calls this again after
/// `/model`, so a model switch re-resolves the full settings (base URL,
/// sampling defaults, ...) rather than only swapping the model id.
/// `prompt_model_fallback` is `-p`/`--prompt-name`'s own `model:`, when set
/// and `-p` was used (`None` from every other caller, including the REPL,
/// which has no `-p` equivalent).
pub(crate) fn resolve_chat_settings(
    shared: &SharedChatArgs,
    prompt_model_fallback: Option<&str>,
    file_config: &ConfigFile,
) -> Result<RequestSettings> {
    let model_name = shared
        .model
        .clone()
        .or_else(|| prompt_model_fallback.map(str::to_owned))
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
        SamplingOverrides {
            reasoning_effort: shared.reasoning_effort,
            temperature: shared.temperature,
            top_p: shared.top_p,
            max_tokens: shared.max_tokens,
        },
        EndpointOverrides {
            base_url: shared.endpoint.base_url.clone(),
            api_key: shared.endpoint.api_key.clone(),
        },
        CapabilityOverrides {
            mcp: (!shared.mcp.is_empty()).then(|| shared.mcp.clone()),
            max_tool_rounds: None,
            // No `--skill` CLI flag: chat only ever gets skills from
            // `default.skills` in `lait.config.yml` (see `resolve_request_settings`).
            skills: None,
            subagents: (!shared.subagent.is_empty()).then(|| shared.subagent.clone()),
            tools: (!shared.tool.is_empty()).then(|| shared.tool.clone()),
        },
        &ModelMap::default(),
        file_config,
    )?;
    Ok(settings.with_usage_label("chat"))
}

/// Resolves `shared.session`'s prior turns (empty when `--session` is unset)
/// into the shape `PromptTurn::history` needs. Shared by `run_chat` and
/// `repl::run`'s startup (the REPL loads history once and grows its own
/// in-memory copy turn by turn from there, rather than reloading from disk
/// every turn).
pub(crate) fn load_session_history(
    session_name: Option<&str>,
) -> Result<Vec<ChatCompletionRequestMessage>> {
    match session_name {
        Some(name) => session::to_request_messages(&session::load(name)?),
        None => Ok(Vec::new()),
    }
}

/// Cancellation-aware counterpart to [`load_session_history`], used by
/// `app::prepare_chat_request` so a `--session` load — reading and
/// deserializing a JSONL file that only grows over the session's lifetime —
/// runs on the same bounded blocking-worker pool as, and concurrently with,
/// the request's other independent reads (file attachments/system prompt/
/// image URLs) instead of blocking ahead of them on the calling task.
pub(crate) async fn load_session_history_cancellable(
    session_name: Option<&str>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Vec<ChatCompletionRequestMessage>> {
    let Some(name) = session_name else {
        return Ok(Vec::new());
    };
    let name = name.to_owned();
    async_io::run_blocking(
        move |_cancelled| session::to_request_messages(&session::load(&name)?),
        cancellation,
    )
    .await
}
