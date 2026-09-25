//! `lait chat`'s interactive REPL: meta-command syntax (`/exit`/`/clear`/
//! `/model`/`/system`/`/undo`/`/retry`/`/usage`/`/help`), `"""`-delimited
//! multi-line input, and the read-eval-print loop itself. Chat-turn
//! settings resolution (`chat::resolve_chat_settings`/`chat::resolve_system_prompt`/
//! `chat::load_session_history`/`chat::finish_chat_turn`) lives in the `chat`
//! module, shared with `app::run_chat`'s single-shot path.

use std::io::Write;

use anyhow::Result;
use async_openai::types::chat::ChatCompletionRequestMessage;

use crate::{
    async_io, chat,
    cli::{ChatReplArgs, SharedChatArgs},
    config::{ConfigFile, ConfigSource},
    engine::{PromptTurn, RequestSettings, RunContext, StreamOptions},
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
    /// `/undo` — drop the last user/assistant exchange from the in-memory
    /// history.
    Undo,
    /// `/retry` — resend the last user message, replacing its reply (or, if
    /// it failed, simply trying it again).
    Retry,
    /// `/usage` — print the token usage accumulated so far this REPL.
    Usage,
    /// `/help` — list the meta commands.
    Help,
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
        "undo" => MetaCommand::Undo,
        "retry" => MetaCommand::Retry,
        "usage" => MetaCommand::Usage,
        "help" => MetaCommand::Help,
        other => MetaCommand::Unknown(other),
    })
}

/// The line that opens and closes a multi-line message: every line typed
/// between two of these is sent as one message, newlines preserved.
const MULTILINE_DELIMITER: &str = "\"\"\"";

const HELP: &str = "\
commands:
  /exit            quit (Ctrl-D also works)
  /clear           reset the in-memory history
  /undo            drop the last exchange from the in-memory history
  /retry           resend the last message
  /model <name>    switch models for later turns
  /system <text>   replace the system prompt
  /usage           show token usage so far
  /help            show this list
  \"\"\"              start/end a multi-line message";

/// The last user message sent, for `/retry`. `in_history` records whether
/// that message's exchange made it into `history` (it succeeded) — `/retry`
/// then has to drop that exchange before resending — or not (it failed, so
/// there is nothing to replace).
struct LastPrompt {
    text: String,
    in_history: bool,
}

/// What the loop should do after a meta command.
enum Next {
    Continue,
    Exit,
    Send(String),
}

/// Runs `lait chat`'s interactive REPL: reads one line at a time from stdin
/// (or a `"""`-delimited block of lines — see `read_message`), sends it
/// (plus every earlier turn this process has seen) to the model, and prints
/// the reply, until `/exit` or end-of-input (Ctrl-D closes stdin, which a
/// piped-stdin test also relies on to end the loop without an explicit
/// `/exit`). The invocation root cancellation is watched while reading and
/// running a turn so Ctrl-C cleans up shared services and child processes.
/// See `parse_meta_command` for the meta-command syntax handled below. Also
/// reached from a prompt-less, stdin-is-a-terminal bare `lait` invocation —
/// see `app::run_chat_or_repl`.
pub(super) async fn run(
    args: ChatReplArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let mut shared = args.shared;
    let file_config = super::load_config(&config_source, &cancel).await?;
    let mut history = chat::load_session_history(shared.session.as_deref())?;
    let mut system_prompt =
        chat::resolve_system_prompt(&shared, &file_config, cancel.clone()).await?;
    let (services, env) =
        super::build_run_context(&file_config, cache_override, approve_tools, cancel.clone());

    eprintln!("lait chat — /help for commands, /exit to quit");

    // Resolved lazily on first use rather than up front, so a `--model`-less
    // invocation still drops into the REPL instead of erroring immediately —
    // the user can `/model <name>` before ever sending a line. Cached across
    // turns after that (`resolve_chat_settings` does only cheap string/config
    // work, but nothing here changes turn to turn except in response to
    // `/model`, which invalidates it below).
    let mut settings: Option<RequestSettings> = None;
    let mut last_prompt: Option<LastPrompt> = None;

    let repl = async {
        loop {
            let Some(message) = read_message(&cancel).await? else {
                break; // end-of-input (Ctrl-D)
            };
            if message.text.is_empty() {
                continue;
            }

            // Only a single typed line can be a meta command; a multi-line
            // block is always a message, even one starting with `/`.
            let command = (!message.multiline)
                .then(|| parse_meta_command(&message.text))
                .flatten();
            let prompt = match command {
                None => message.text,
                Some(command) => match apply_meta_command(
                    command,
                    &mut history,
                    &mut shared,
                    &mut settings,
                    &mut system_prompt,
                    &mut last_prompt,
                    &env,
                ) {
                    Next::Exit => break,
                    Next::Continue => continue,
                    Next::Send(prompt) => prompt,
                },
            };

            let Some(settings) = ensure_settings(&mut settings, &shared, &file_config) else {
                continue;
            };

            match run_turn(
                settings,
                &env,
                &system_prompt,
                &history,
                &prompt,
                shared.show_reasoning,
                shared.reporting.show_usage,
            )
            .await
            {
                Ok((assistant_text, turn_usage)) => {
                    history.push(llm::user_message(&prompt, &[])?);
                    history.push(llm::assistant_message(&assistant_text)?);
                    chat::finish_chat_turn(
                        shared.session.as_deref(),
                        shared.reporting.no_history,
                        &file_config,
                        &settings.resolved_model.model_id,
                        &prompt,
                        &assistant_text,
                        turn_usage,
                    )?;
                    last_prompt = Some(LastPrompt {
                        text: prompt,
                        in_history: true,
                    });
                }
                // One bad turn (a request error, a bad `/model` name that only
                // fails once actually resolved) shouldn't end the whole session
                // — report it and let the user try again or `/exit`.
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => {
                    eprintln!("lait: {error:#}");
                    last_prompt = Some(LastPrompt {
                        text: prompt,
                        in_history: false,
                    });
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    services.finish(repl).await
}

/// One message read by [`read_message`].
struct Message {
    text: String,
    /// Whether `text` came from a `"""` block rather than a single line.
    multiline: bool,
}

/// Reads one message: normally a single trimmed line, but a line consisting
/// only of `"""` starts a multi-line block that runs until the next such
/// line, returned with its inner lines joined by `\n` (a block cut short by
/// end-of-input still sends what was typed). `None` at end-of-input.
async fn read_message(cancel: &tokio_util::sync::CancellationToken) -> Result<Option<Message>> {
    let Some(line) = read_line("> ", cancel).await? else {
        return Ok(None);
    };
    let line = line.trim();
    if line != MULTILINE_DELIMITER {
        return Ok(Some(Message {
            text: line.to_owned(),
            multiline: false,
        }));
    }
    let mut lines = Vec::new();
    while let Some(line) = read_line("... ", cancel).await? {
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim() == MULTILINE_DELIMITER {
            break;
        }
        lines.push(line.to_owned());
    }
    Ok(Some(Message {
        text: lines.join("\n").trim().to_owned(),
        multiline: true,
    }))
}

/// Prints `prompt` to stderr and reads one raw line from stdin on a
/// cancellable blocking thread. `None` at end-of-input.
async fn read_line(
    prompt: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<String>> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let (bytes_read, line) = async_io::run_blocking(
        move |_cancelled| {
            let mut line = String::new();
            let bytes_read = std::io::stdin().read_line(&mut line)?;
            Ok((bytes_read, line))
        },
        cancel.clone(),
    )
    .await?;
    Ok((bytes_read != 0).then_some(line))
}

/// Applies one parsed [`MetaCommand`], mutating REPL state as needed and
/// printing a status line. `/retry` is the one command that sends a
/// message, which it hands back as [`Next::Send`] for the loop to run like
/// any typed line.
fn apply_meta_command(
    command: MetaCommand<'_>,
    history: &mut Vec<ChatCompletionRequestMessage>,
    shared: &mut SharedChatArgs,
    settings: &mut Option<RequestSettings>,
    system_prompt: &mut Option<String>,
    last_prompt: &mut Option<LastPrompt>,
    env: &RunContext,
) -> Next {
    match command {
        MetaCommand::Exit => return Next::Exit,
        MetaCommand::Clear => {
            history.clear();
            *last_prompt = None;
            eprintln!("(history cleared — a --session log, if any, is unaffected)");
        }
        MetaCommand::Model(name) if !name.is_empty() => {
            shared.model = Some(name.to_owned());
            *settings = None;
            eprintln!("(model set to '{name}')");
        }
        MetaCommand::Model(_) => eprintln!("usage: /model <name>"),
        MetaCommand::System(text) if !text.is_empty() => {
            *system_prompt = Some(text.to_owned());
            eprintln!("(system prompt updated)");
        }
        MetaCommand::System(_) => eprintln!("usage: /system <text>"),
        MetaCommand::Undo => {
            if history.len() >= 2 {
                history.truncate(history.len() - 2);
                *last_prompt = None;
                eprintln!("(last exchange removed — a --session log, if any, is unaffected)");
            } else {
                eprintln!("(nothing to undo)");
            }
        }
        MetaCommand::Retry => match last_prompt.take() {
            Some(LastPrompt { text, in_history }) => {
                if in_history {
                    history.truncate(history.len().saturating_sub(2));
                }
                eprintln!("(retrying: {})", first_line(&text));
                return Next::Send(text);
            }
            None => eprintln!("(nothing to retry)"),
        },
        MetaCommand::Usage => usage::print_usage_summary(&env.usage),
        MetaCommand::Help => eprintln!("{HELP}"),
        MetaCommand::Unknown(name) => eprintln!("unknown command: /{name} (see /help)"),
    }
    Next::Continue
}

/// `text`'s first line, marked with `…` when there was more — for a
/// one-line status message about a possibly multi-line prompt.
fn first_line(text: &str) -> String {
    match text.split_once('\n') {
        Some((first, _)) => format!("{first}…"),
        None => text.to_owned(),
    }
}

/// Resolves `*settings` if unset — invalidated by `/model`, or never set on
/// this REPL's first turn — and returns a reference to it. Resolved lazily
/// like this (rather than up front) so a `--model`-less invocation still
/// drops into the REPL instead of erroring immediately, and cached across
/// turns after that since nothing here changes turn to turn except in
/// response to `/model`. Returns `None` when resolution itself failed (a bad
/// `/model` name, no model set at all) — reported here so the caller can
/// simply `continue` its loop rather than needing its own error-handling
/// branch or an `.expect()` on an invariant this function already keeps.
fn ensure_settings<'a>(
    settings: &'a mut Option<RequestSettings>,
    shared: &SharedChatArgs,
    file_config: &ConfigFile,
) -> Option<&'a RequestSettings> {
    if settings.is_none() {
        match chat::resolve_chat_settings(shared, None, file_config) {
            Ok(resolved) => *settings = Some(resolved),
            Err(error) => {
                eprintln!("lait: {error:#}");
                return None;
            }
        }
    }
    settings.as_ref()
}

/// Runs one `lait chat` turn: streams the response to stdout, driving the
/// same MCP/subagent tool loop `RequestSettings::complete` does when
/// `settings.mcp`/`settings.subagents` names at least one tool source (see
/// `RequestSettings::complete_stream`). Returns the assistant's raw reply
/// text (never the `Reasoning:`-prefixed display form, the shape
/// `history`/a `--session` log need) alongside this turn's own token usage
/// (not `env.usage`'s running session total — see the `before`/`after`
/// delta below — since `env` persists across every REPL turn and `lait
/// history` wants each entry's own usage, not the cumulative session total).
async fn run_turn(
    settings: &RequestSettings,
    env: &RunContext,
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
        media: &[],
    };
    let outcome = settings
        .complete_stream(
            env,
            &[],
            turn,
            None,
            StreamOptions {
                include_usage: show_usage,
                show_reasoning,
                output_path: None,
            },
            env.operation_token(),
        )
        .await?;
    // Recorded even without `--show-usage` (a server may report usage
    // unasked), so `/usage` can show whatever is known.
    if let Some(usage) = outcome.usage {
        env.usage.record(
            &settings.usage_label,
            usage,
            settings.resolved_model.pricing,
        );
    }
    if show_usage {
        usage::print_usage_summary(&env.usage);
    }
    let content = outcome.content;
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
    fn parses_argument_free_commands() {
        assert_eq!(parse_meta_command("/undo"), Some(MetaCommand::Undo));
        assert_eq!(parse_meta_command("/retry"), Some(MetaCommand::Retry));
        assert_eq!(parse_meta_command("/usage"), Some(MetaCommand::Usage));
        assert_eq!(parse_meta_command("/help"), Some(MetaCommand::Help));
    }

    #[test]
    fn first_line_marks_truncation() {
        assert_eq!(super::first_line("one"), "one");
        assert_eq!(super::first_line("one\ntwo"), "one…");
    }

    #[test]
    fn parses_an_unrecognized_command() {
        assert_eq!(
            parse_meta_command("/nope"),
            Some(MetaCommand::Unknown("nope"))
        );
    }
}
