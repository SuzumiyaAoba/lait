//! `lait chat`'s interactive REPL: meta-command syntax (`/exit`/`/clear`/
//! `/model`/`/system`) and the read-eval-print loop itself. Chat-turn
//! settings resolution (`resolve_chat_settings`/`resolve_system_prompt`/
//! `load_session_history`/`finish_chat_turn`) stays in `app`, shared with
//! `run_chat`'s single-shot path.

use std::{
    io::{BufRead, Write},
    sync::Arc,
};

use anyhow::Result;
use async_openai::types::chat::ChatCompletionRequestMessage;

use crate::{
    app,
    cli::ChatReplArgs,
    config,
    engine::{AppContext, PromptTurn, RequestSettings, stream_response},
    llm, response, usage,
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MetaCommand<'a> {
    /// `/exit` — end the REPL.
    Exit,
    /// `/clear` — drop the in-memory conversation history.
    Clear,
    /// `/model <name>` — switch models for subsequent turns. Empty when the
    /// line had no argument (`/model` alone), which the caller reports as a
    /// usage error rather than silently clearing the model.
    Model(&'a str),
    /// `/system <text>` — replace the system prompt for subsequent turns.
    /// Empty for the same reason as `Model` above.
    System(&'a str),
    /// A `/`-prefixed line that isn't one of the commands above.
    Unknown(&'a str),
}

/// Parses one line of REPL input for a `/`-prefixed meta command. Returns
/// `None` when `line` isn't a meta command at all (an ordinary chat message
/// to send to the model), so the caller can tell "not a command" apart from
/// `Some(MetaCommand::Unknown(_))` ("looked like a command, but not one lait
/// knows").
pub(crate) fn parse_meta_command(line: &str) -> Option<MetaCommand<'_>> {
    let rest = line.strip_prefix('/')?;
    let (command, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let argument = argument.trim();
    Some(match command {
        "exit" => MetaCommand::Exit,
        "clear" => MetaCommand::Clear,
        "model" => MetaCommand::Model(argument),
        "system" => MetaCommand::System(argument),
        other => MetaCommand::Unknown(other),
    })
}

/// Runs `lait chat`'s interactive REPL: reads one line at a time from stdin,
/// sends it (plus every earlier turn this process has seen) to the model,
/// and prints the reply, until `/exit` or end-of-input (Ctrl-D closes stdin,
/// which a piped-stdin test also relies on to end the loop without an
/// explicit `/exit`). See `parse_meta_command` for the `/exit`/`/clear`/
/// `/model`/`/system` syntax handled below. Also reached from a prompt-less,
/// stdin-is-a-terminal bare `lait` invocation — see `app::run_chat_or_repl`.
pub(crate) async fn run(args: ChatReplArgs, no_config: bool) -> Result<()> {
    let mut shared = args.shared;
    let file_config = Arc::new(config::load_config(no_config)?);
    let mut history = app::load_session_history(shared.session.as_deref())?;
    let mut system_prompt = app::resolve_system_prompt(&shared, &file_config)?;
    let env = AppContext::new(Arc::clone(&file_config));

    eprintln!("lait chat — /exit to quit, /clear to reset history, /model <name>, /system <text>");

    // Resolved lazily on first use rather than up front, so a `--model`-less
    // invocation still drops into the REPL instead of erroring immediately —
    // the user can `/model <name>` before ever sending a line. Cached across
    // turns after that (`resolve_chat_settings` does only cheap string/config
    // work, but nothing here changes turn to turn except in response to
    // `/model`, which invalidates it below).
    let mut settings: Option<RequestSettings> = None;

    let stdin = std::io::stdin();
    let repl = async {
        loop {
            eprint!("> ");
            std::io::stderr().flush()?;
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                break; // end-of-input (Ctrl-D)
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(command) = parse_meta_command(line) {
                match command {
                    MetaCommand::Exit => break,
                    MetaCommand::Clear => {
                        history.clear();
                        eprintln!("(history cleared — a --session log, if any, is unaffected)");
                    }
                    MetaCommand::Model(name) if !name.is_empty() => {
                        shared.model = Some(name.to_owned());
                        settings = None;
                        eprintln!("(model set to '{name}')");
                    }
                    MetaCommand::Model(_) => eprintln!("usage: /model <name>"),
                    MetaCommand::System(text) if !text.is_empty() => {
                        system_prompt = Some(text.to_owned());
                        eprintln!("(system prompt updated)");
                    }
                    MetaCommand::System(_) => eprintln!("usage: /system <text>"),
                    MetaCommand::Unknown(name) => eprintln!("unknown command: /{name}"),
                }
                continue;
            }

            if settings.is_none() {
                settings = match app::resolve_chat_settings(&shared, None, &file_config) {
                    Ok(resolved) => Some(resolved),
                    Err(error) => {
                        eprintln!("lait: {error:#}");
                        continue;
                    }
                };
            }
            let settings = settings
                .as_ref()
                .expect("just resolved above, or the loop continued before reaching here");

            match run_turn(
                settings,
                &env,
                &system_prompt,
                &history,
                line,
                shared.show_reasoning,
                shared.reporting.show_usage,
            )
            .await
            {
                Ok((assistant_text, turn_usage)) => {
                    history.push(llm::user_message(line, &[])?);
                    history.push(llm::assistant_message(&assistant_text)?);
                    app::finish_chat_turn(
                        shared.session.as_deref(),
                        shared.reporting.no_history,
                        &file_config,
                        &settings.resolved_model.model_id,
                        line,
                        &assistant_text,
                        turn_usage,
                    )?;
                }
                // One bad turn (a request error, a bad `/model` name that only
                // fails once actually resolved) shouldn't end the whole session
                // — report it and let the user try again or `/exit`.
                Err(error) => eprintln!("lait: {error:#}"),
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    env.finish(repl).await
}

/// Runs one `lait chat` turn: streams the response to stdout (the REPL's
/// default), or — when `settings.mcp`/`settings.subagents` names at least
/// one tool source — falls back to a single non-streamed request printed
/// once it completes, since `RequestSettings::complete_stream` cannot yet
/// drive a tool loop (see its own doc comment). Returns the assistant's raw
/// reply text (never the `Reasoning:`-prefixed display form, the shape
/// `history`/a `--session` log need) alongside this turn's own token usage
/// (not `env.usage`'s running session total — see the `before`/`after`
/// delta below — since `env` persists across every REPL turn and `lait
/// history` wants each entry's own usage, not the cumulative session total).
async fn run_turn(
    settings: &RequestSettings,
    env: &AppContext,
    system_prompt: &Option<String>,
    history: &[ChatCompletionRequestMessage],
    prompt: &str,
    show_reasoning: bool,
    show_usage: bool,
) -> Result<(String, Option<response::Usage>)> {
    let before = env.usage.total().unwrap_or_default();
    let turn = PromptTurn {
        system_prompt: system_prompt.as_deref(),
        history,
        prompt,
        image_urls: &[],
    };
    let content = if settings.mcp.is_empty() && settings.subagents.is_empty() {
        let stream = settings
            .complete_stream(&env.skill_cache, turn, None, show_usage)
            .await?;
        let outcome = stream_response(stream, show_reasoning, None).await?;
        if show_usage && let Some(usage) = outcome.usage {
            env.usage.record(&settings.usage_label, usage);
        }
        if show_usage {
            usage::print_usage_summary(&env.usage);
        }
        outcome.content
    } else {
        let response = settings
            .complete(env, &[], turn, None, env.cancel.clone())
            .await?;
        let rendered = response::render_response(&response, false, show_reasoning)?;
        println!("{rendered}");
        if show_usage {
            usage::print_usage_summary(&env.usage);
        }
        response::content_text(&response).to_owned()
    };
    let turn_usage = env.usage.total().map(|after| response::Usage {
        prompt_tokens: after.prompt_tokens.saturating_sub(before.prompt_tokens),
        completion_tokens: after
            .completion_tokens
            .saturating_sub(before.completion_tokens),
        total_tokens: after.total_tokens.saturating_sub(before.total_tokens),
    });
    Ok((content, turn_usage))
}

#[cfg(test)]
mod tests {
    use super::{MetaCommand, parse_meta_command};

    #[test]
    fn returns_none_for_an_ordinary_message() {
        assert_eq!(parse_meta_command("hello there"), None);
        assert_eq!(parse_meta_command(""), None);
    }

    #[test]
    fn parses_exit_and_clear() {
        assert_eq!(parse_meta_command("/exit"), Some(MetaCommand::Exit));
        assert_eq!(parse_meta_command("/clear"), Some(MetaCommand::Clear));
    }

    #[test]
    fn parses_model_and_system_with_an_argument() {
        assert_eq!(
            parse_meta_command("/model gpt-oss-20b"),
            Some(MetaCommand::Model("gpt-oss-20b"))
        );
        assert_eq!(
            parse_meta_command("/system You are terse."),
            Some(MetaCommand::System("You are terse."))
        );
    }

    #[test]
    fn parses_model_and_system_with_no_argument_as_empty() {
        assert_eq!(parse_meta_command("/model"), Some(MetaCommand::Model("")));
        assert_eq!(parse_meta_command("/model  "), Some(MetaCommand::Model("")));
        assert_eq!(parse_meta_command("/system"), Some(MetaCommand::System("")));
    }

    #[test]
    fn parses_an_unrecognized_command() {
        assert_eq!(
            parse_meta_command("/nope"),
            Some(MetaCommand::Unknown("nope"))
        );
    }
}
