//! Conversation sessions (`--session`/`lait sessions`, see
//! `docs/usage/ja/chat.md`): a named, project-local, append-only JSONL log of
//! user/assistant turns that lets a single-shot `lait` invocation (or a
//! `lait chat` REPL) continue a conversation across separate process runs.
//! System prompts are never persisted here — they're re-supplied on every
//! call (`--system`/`--system-file`/`default.system`), same as without a
//! session.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use async_openai::types::chat::ChatCompletionRequestMessage;
use serde::{Deserialize, Serialize};

use crate::{
    cli::{SessionsAction, SessionsCommand},
    jsonl, llm,
};

/// The directory every session's JSONL file lives under, relative to the
/// current directory (a project-local concept, unlike the user-wide
/// `--session`-less `lait history`).
const SESSIONS_DIR: &str = ".lait/sessions";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredMessage {
    pub(crate) role: Role,
    pub(crate) content: String,
}

/// Rejects a session name that isn't a plain identifier, so `--session`/
/// `lait sessions <action>` can never be tricked into reading or writing a
/// file outside `SESSIONS_DIR` (e.g. via `..` or a `/`-containing name).
pub(crate) fn validate_name(name: &str) -> Result<()> {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Ok(());
    }
    bail!(
        "invalid session name '{name}'; session names may only contain letters, digits, '_', and '-'"
    )
}

fn session_path(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(Path::new(SESSIONS_DIR).join(format!("{name}.jsonl")))
}

/// Loads every turn recorded for session `name`, in the order they were
/// appended. Returns an empty `Vec` (not an error) when the session has never
/// been used before — starting a brand-new named session is the common case
/// for a first `--session <NAME>` call.
pub(crate) fn load(name: &str) -> Result<Vec<StoredMessage>> {
    jsonl::load(&session_path(name)?)
}

/// Converts a loaded session's turns into the message shape
/// `llm::initial_messages`'s `history` parameter expects.
pub(crate) fn to_request_messages(
    messages: &[StoredMessage],
) -> Result<Vec<ChatCompletionRequestMessage>> {
    messages
        .iter()
        .map(|message| match message.role {
            Role::User => llm::user_message(&message.content, &[]),
            Role::Assistant => llm::assistant_message(&message.content),
        })
        .collect()
}

/// Appends one completed turn (a user message and the assistant's reply) to
/// session `name`, creating the session directory/file on first use. Only
/// ever called once a request has actually succeeded, so a failed turn never
/// leaves a dangling user-only message a later resume would send back to the
/// model without its answer.
pub(crate) fn append_turn(name: &str, user_content: &str, assistant_content: &str) -> Result<()> {
    let path = session_path(name)?;
    jsonl::append(
        &path,
        [
            StoredMessage {
                role: Role::User,
                content: user_content.to_owned(),
            },
            StoredMessage {
                role: Role::Assistant,
                content: assistant_content.to_owned(),
            },
        ],
    )
}

/// The number of turns (user+assistant message pairs) recorded in the
/// session file at `path`, counted from raw lines rather than deserializing
/// every stored message — `lait sessions list` only needs the count.
fn count_turns(path: &Path) -> Result<usize> {
    Ok(jsonl::count_lines(path)? / 2)
}

/// One row of `lait sessions list`: a session's name and how many turns
/// (user+assistant message pairs) it holds.
pub(crate) struct SessionSummary {
    pub(crate) name: String,
    pub(crate) turn_count: usize,
}

/// Lists every session under `SESSIONS_DIR`, sorted by name. Returns an empty
/// `Vec` when the directory doesn't exist yet (no session has ever been
/// created in this project).
pub(crate) fn list() -> Result<Vec<SessionSummary>> {
    let dir = Path::new(SESSIONS_DIR);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read session directory '{}'", dir.display()));
        }
    };

    let mut summaries = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to read an entry of session directory '{}'",
                dir.display()
            )
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        summaries.push(SessionSummary {
            name: name.to_owned(),
            turn_count: count_turns(&path)?,
        });
    }
    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(summaries)
}

/// Loads session `name` for `lait sessions show`, failing with a clear error
/// (rather than `load`'s "empty session" fallback) when it doesn't exist —
/// `show`ing a session that was never created is a user mistake worth
/// reporting, unlike resuming one via `--session` for the first time.
pub(crate) fn show(name: &str) -> Result<Vec<StoredMessage>> {
    let path = session_path(name)?;
    if !path.exists() {
        bail!("no such session '{name}'");
    }
    load(name)
}

/// Deletes session `name`'s file. Fails with a clear error when it doesn't
/// exist.
pub(crate) fn delete(name: &str) -> Result<()> {
    let path = session_path(name)?;
    fs::remove_file(&path).with_context(|| match path.exists() {
        true => format!("failed to delete session file '{}'", path.display()),
        false => format!("no such session '{name}'"),
    })
}

/// Runs `lait sessions <action>`: `list`/`show <name>`/`delete <name>`, all
/// purely local file operations (no async runtime needed — see
/// `app::needs_async_runtime`).
pub(crate) fn run(command: SessionsCommand) -> Result<()> {
    match command.action {
        SessionsAction::List => {
            let summaries = list()?;
            if summaries.is_empty() {
                println!("no sessions saved yet; start one with `lait --session <NAME> ...`");
                return Ok(());
            }
            for summary in summaries {
                println!("{}  ({} turns)", summary.name, summary.turn_count);
            }
            Ok(())
        }
        SessionsAction::Show(args) => {
            for message in show(&args.name)? {
                let role = match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                println!("{role}: {}", message.content);
            }
            Ok(())
        }
        SessionsAction::Delete(args) => {
            delete(&args.name)?;
            println!("deleted session '{}'", args.name);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Role, StoredMessage, append_turn, delete, list, load, show, validate_name};

    /// Runs `body` with the current directory temporarily switched to a
    /// fresh, empty directory, so `SESSIONS_DIR` ("`.lait/sessions`") resolves
    /// under an isolated location instead of this repository's own working
    /// tree. Serialized via a global lock: the process-wide current directory
    /// is shared mutable state, and `cargo test`'s default threaded runner
    /// would otherwise let two session tests race on it.
    fn in_temp_dir<T>(body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let dir = std::env::temp_dir().join(format!(
            "lait-test-session-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let result = body();
        std::env::set_current_dir(original).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn validate_name_accepts_plain_identifiers() {
        assert!(validate_name("demo").is_ok());
        assert!(validate_name("demo-1_2").is_ok());
    }

    #[test]
    fn validate_name_rejects_path_like_names() {
        assert!(validate_name("").is_err());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a b").is_err());
    }

    #[test]
    fn load_returns_empty_for_a_session_that_has_never_been_used() {
        in_temp_dir(|| {
            assert!(load("brand-new").unwrap().is_empty());
        });
    }

    #[test]
    fn append_turn_then_load_round_trips_the_conversation() {
        in_temp_dir(|| {
            append_turn("demo", "hello", "hi there").unwrap();
            append_turn("demo", "how are you?", "doing well").unwrap();

            let messages = load("demo").unwrap();
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[0].role, Role::User);
            assert_eq!(messages[0].content, "hello");
            assert_eq!(messages[1].role, Role::Assistant);
            assert_eq!(messages[1].content, "hi there");
            assert_eq!(messages[2].content, "how are you?");
            assert_eq!(messages[3].content, "doing well");
        });
    }

    #[test]
    fn list_reports_every_session_with_its_turn_count() {
        in_temp_dir(|| {
            append_turn("a", "1", "2").unwrap();
            append_turn("b", "1", "2").unwrap();
            append_turn("b", "3", "4").unwrap();

            let summaries = list().unwrap();
            assert_eq!(summaries.len(), 2);
            assert_eq!(summaries[0].name, "a");
            assert_eq!(summaries[0].turn_count, 1);
            assert_eq!(summaries[1].name, "b");
            assert_eq!(summaries[1].turn_count, 2);
        });
    }

    #[test]
    fn list_returns_empty_when_no_session_has_ever_been_created() {
        in_temp_dir(|| {
            assert!(list().unwrap().is_empty());
        });
    }

    #[test]
    fn show_fails_clearly_for_a_missing_session() {
        in_temp_dir(|| {
            let error = show("missing").unwrap_err();
            assert!(error.to_string().contains("no such session"));
        });
    }

    #[test]
    fn show_returns_the_recorded_messages() {
        in_temp_dir(|| {
            append_turn("demo", "hi", "hello").unwrap();
            let messages = show("demo").unwrap();
            assert_eq!(messages.len(), 2);
        });
    }

    #[test]
    fn delete_removes_the_session_file() {
        in_temp_dir(|| {
            append_turn("demo", "hi", "hello").unwrap();
            delete("demo").unwrap();
            assert!(load("demo").unwrap().is_empty());
        });
    }

    #[test]
    fn delete_fails_clearly_for_a_missing_session() {
        in_temp_dir(|| {
            let error = delete("missing").unwrap_err();
            assert!(error.to_string().contains("no such session"));
        });
    }

    #[test]
    fn stored_message_json_shape_uses_lowercase_roles() {
        let message = StoredMessage {
            role: Role::User,
            content: "hi".to_owned(),
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"user","content":"hi"}"#
        );
    }
}
