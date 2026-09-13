//! `lait`'s bare-invocation chat entry point (`lait [OPTIONS] [PROMPT]`) and
//! `lait chat`'s single-shot request path: resolving a chat turn's
//! settings/history/attachments/display policy and sending it, streamed or
//! not. Split out of `app.rs` to keep that module to pure dispatch (see its
//! own doc comment) — `run`'s `AsyncCommand::Bare` arm is this module's only
//! caller.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_openai::types::chat::{ChatCompletionRequestMessage, ResponseFormat};

use crate::{
    attachment, chat,
    cli::{ChatArgs, ChatReplArgs},
    config::{self, ConfigSource},
    engine::{AppServices, PromptTurn, RequestSettings, RunContext, StreamOptions},
    prompt, repl, report, response, schema, usage,
};

use super::build_run_context;

/// The bare-invocation entry point (`lait [OPTIONS] [PROMPT]`, no
/// subcommand): sends a single-shot chat request when a prompt is available
/// (an argument or piped stdin — see `chat::resolve_input_with_stdin_cancellable`), or, when
/// none is and stdin is an interactive terminal, starts the same REPL
/// `lait chat` does instead of erroring. Piped-but-empty stdin (a script's
/// `< /dev/null`, or a forgotten argument in a pipeline) still errors exactly
/// as before — only an actual interactive terminal with nothing typed counts
/// as "the user wants the REPL", so a script's exit-code contract never
/// silently changes into "launched an interactive prompt that then exits
/// immediately."
pub(super) async fn run_chat_or_repl(
    chat: ChatArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use std::io::IsTerminal;

    // The REPL installs its own listener once it starts. Every other bare
    // invocation must install one before the cancellable initial stdin read
    // (or before `run_chat`'s config load).
    let enters_repl = chat.prompt.is_none() && std::io::stdin().is_terminal();
    if !enters_repl {
        crate::signal::spawn_handler(cancel.clone());
    }
    match chat::resolve_input_with_stdin_cancellable(chat.prompt.clone(), Some(cancel.clone()))
        .await?
    {
        Some(prompt) => {
            run_chat(
                chat,
                prompt,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
        None if std::io::stdin().is_terminal() => {
            repl::run(
                ChatReplArgs {
                    shared: chat.shared,
                },
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )
            .await
        }
        None => Err(anyhow!(
            "a PROMPT is required; provide one, pipe input via stdin, or use `lait run <FILE> <PROMPT>`"
        )),
    }
}

/// The tail shared by both of `run_chat`'s branches: records the turn to
/// session/history and prints the usage summary when asked. `content` is
/// the streamed outcome's accumulated text or the non-streamed response's
/// rendered content, whichever branch reached here — the streamed branch
/// still records the response's usage into `env.usage` itself first (see
/// its own comment on why that can't move here).
fn finish_chat_run(
    chat: &ChatArgs,
    file_config: &config::ConfigFile,
    model_id: &str,
    prompt: &str,
    content: &str,
    env: &RunContext,
    show_usage: bool,
) -> Result<()> {
    chat::finish_chat_turn(
        chat.shared.session.as_deref(),
        chat.shared.reporting.no_history,
        file_config,
        model_id,
        prompt,
        content,
        env.usage.total(),
    )?;
    if show_usage {
        usage::print_usage_summary(&env.usage);
    }
    Ok(())
}

/// Runs a single-shot chat request with an already-resolved `prompt` — see
/// `run_chat_or_repl`, the only caller, for how `prompt` was resolved (a
/// CLI argument and/or piped stdin).
/// Display/output policy derived from `--quiet`/`--show-reasoning`/
/// `--show-usage`/`--render`/`-o`, shared by [`run_chat`]'s streaming and
/// non-streaming completion paths.
struct ChatDisplayPolicy<'a> {
    show_reasoning: bool,
    show_usage: bool,
    render_enabled: bool,
    /// `-o -` is an explicit "stdout", the same as no `-o` at all.
    output_path: Option<&'a std::path::Path>,
}

impl<'a> ChatDisplayPolicy<'a> {
    fn resolve(chat: &'a ChatArgs, file_config: &config::ConfigFile) -> Self {
        // `--quiet` keeps the response body and drops every note around it.
        Self {
            show_reasoning: chat.shared.show_reasoning && !chat.quiet,
            show_usage: chat.shared.reporting.show_usage && !chat.quiet,
            render_enabled: chat
                .output
                .render_enabled(file_config.default.render.unwrap_or(false)),
            output_path: chat.output.output_path(),
        }
    }
}

/// Everything [`run_chat`] needs to send its one completion request, built
/// by [`prepare_chat_request`]: config, resolved model/sampling settings,
/// the fully assembled prompt (template-rendered and file-attachment-
/// appended), and the run's services/context/display policy.
struct ChatRequest<'a> {
    file_config: Arc<config::ConfigFile>,
    settings: RequestSettings,
    response_format: Option<ResponseFormat>,
    prompt: String,
    system_prompt: Option<String>,
    session_history: Vec<ChatCompletionRequestMessage>,
    image_urls: Vec<String>,
    services: Arc<AppServices>,
    env: RunContext,
    display: ChatDisplayPolicy<'a>,
}

/// Resolves everything a chat completion request needs, ahead of actually
/// sending it: `-p`/`--prompt-name` template rendering, model/sampling
/// settings, then the JSON Schema, file attachments, system prompt, image
/// URLs, and session history (all read concurrently — see the
/// `tokio::try_join!` below), and finally the run's services/context.
async fn prepare_chat_request<'a>(
    chat: &'a ChatArgs,
    prompt: String,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<ChatRequest<'a>> {
    let file_config =
        Arc::new(config::load_config_cancellable(&config_source, Some(cancel.clone())).await?);

    // `-p`/`--prompt-name` renders a named `prompts:` template against
    // `prompt` (which, for this path, is really the template's `{{ input }}`
    // rather than literal text to send) before anything else touches it —
    // `--file` attachments below still append to the *rendered* text, the
    // same way they'd append to a plain prompt.
    let (prompt, prompt_model_fallback) = match &chat.prompt_name {
        Some(name) => prompt::render_named(name, &prompt, &chat.var.var, &file_config)?,
        None => (prompt, None),
    };

    let settings =
        chat::resolve_chat_settings(&chat.shared, prompt_model_fallback.as_deref(), &file_config)?;

    // These five reads are independent of each other and of everything
    // above: the JSON Schema only needs `chat.json_schema`/`chat.schema_name`,
    // file attachments only need `chat.files`, the system prompt only needs
    // `chat.shared`/`file_config`, image URLs only need `chat.images`, and
    // the session history only needs `chat.shared.session`. Running them
    // concurrently rather than one after another (as `workflow/exec.rs`
    // already does for its own file+image pair via `tokio::try_join!`)
    // shortens the wall-clock delay before the first token on e.g.
    // `lait -f a.rs -f b.rs --image x.png --json-schema s.json "..."`.
    //
    // Behavior note: when two of these fail at once, which error surfaces
    // is now whichever `try_join!` polls to a `Result::Err` first rather
    // than a fixed left-to-right order (`try_join!` itself cancels the
    // remaining futures on the first error but does not guarantee polling
    // order across unrelated futures) — none of these five reads name each
    // other in their error text, so there is no risk of a confusing partial
    // message, only a different tie-break among independent failures.
    let (response_format, file_context, system_prompt, image_urls, session_history) = tokio::try_join!(
        async {
            match chat.json_schema.as_deref() {
                Some(path) => schema::load_json_schema_cancellable(
                    path,
                    &chat.schema_name,
                    Some(cancel.clone()),
                )
                .await
                .map(Some),
                None => Ok(None),
            }
        },
        attachment::read_file_attachments(&chat.files),
        chat::resolve_system_prompt(&chat.shared, &file_config, Some(cancel.clone())),
        attachment::resolve_image_urls(&chat.images),
        chat::load_session_history_cancellable(
            chat.shared.session.as_deref(),
            Some(cancel.clone())
        ),
    )?;
    let prompt = match file_context {
        Some(file_context) => format!("{prompt}\n\n{file_context}"),
        None => prompt,
    };
    let (services, env) = build_run_context(&file_config, cache_override, approve_tools, cancel);
    let display = ChatDisplayPolicy::resolve(chat, &file_config);

    Ok(ChatRequest {
        file_config,
        settings,
        response_format,
        prompt,
        system_prompt,
        session_history,
        image_urls,
        services,
        env,
        display,
    })
}

async fn run_chat(
    chat: ChatArgs,
    prompt: String,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let ChatRequest {
        file_config,
        settings,
        response_format,
        prompt,
        system_prompt,
        session_history,
        image_urls,
        services,
        env,
        display,
    } = prepare_chat_request(
        &chat,
        prompt,
        config_source,
        cache_override,
        approve_tools,
        cancel,
    )
    .await?;

    let turn = PromptTurn {
        system_prompt: system_prompt.as_deref(),
        history: &session_history,
        prompt: &prompt,
        image_urls: &image_urls,
    };

    if chat.stream {
        let outcome = services
            .finish(settings.complete_stream(
                &env,
                &[],
                turn,
                response_format,
                StreamOptions {
                    include_usage: display.show_usage,
                    show_reasoning: display.show_reasoning,
                    output_path: display.output_path,
                },
                Some(env.operation_token()),
            ))
            .await?;
        // Streamed usage arrives on the final chunk rather than through
        // `complete`; feed it into the same tally so both chat paths share
        // one summary format and so `env.usage.total()` below reflects it.
        if let Some(usage) = outcome.usage {
            env.usage.record(&settings.usage_label, usage);
        }
        return finish_chat_run(
            &chat,
            &file_config,
            &settings.resolved_model.model_id,
            &prompt,
            &outcome.content,
            &env,
            display.show_usage,
        );
    }

    let response = services
        .finish(settings.complete(
            &env,
            &[],
            turn,
            response_format,
            Some(env.operation_token()),
        ))
        .await?;

    match display.output_path {
        Some(path) => {
            // The file gets the body alone; reasoning, when requested,
            // becomes a stderr note like usage.
            if display.show_reasoning
                && let Some(reasoning) = response::response_reasoning(&response)
            {
                eprintln!("Reasoning:\n{reasoning}\n");
            }
            let body = response::render_response(
                &response,
                response::RenderOptions {
                    as_json: chat.output.json,
                    show_reasoning: false,
                },
            )?;
            report::emit_output(&body, Some(path), false)?;
        }
        None => {
            let output = response::render_response(
                &response,
                response::RenderOptions {
                    as_json: chat.output.json,
                    show_reasoning: display.show_reasoning,
                },
            )?;
            // `--json`'s output is machine-readable and never rendered as
            // Markdown; `chat.stream`'s branch above already returned before
            // reaching here, so `--render` never has to reckon with a
            // partial streamed response either — see `report::maybe_render`.
            report::emit_output(&output, None, !chat.output.json && display.render_enabled)?;
        }
    }
    let content = response::content_text(&response);
    finish_chat_run(
        &chat,
        &file_config,
        &settings.resolved_model.model_id,
        &prompt,
        content,
        &env,
        display.show_usage,
    )
}
