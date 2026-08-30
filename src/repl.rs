//! `lait chat`'s meta-command syntax (`app::run_chat_repl` drives the actual
//! REPL loop — it needs `app`'s private request-building machinery, so it
//! lives there; this module only holds the pure, easily unit-tested parsing
//! of a `/`-prefixed line).

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
