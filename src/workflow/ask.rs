//! `type: ask` — a human-in-the-loop workflow node (see
//! `model::AskNode`/docs/usage/ja/workflow.md). `run_ask` is the only
//! entry point; everything else here is a pure helper kept separately
//! testable from the actual stdin I/O.

use std::io::{BufRead, IsTerminal, Read};

use anyhow::{Context, Result, bail};

use crate::async_io;

use super::model::AskNode;

/// Prompts (already-rendered `prompt` text) and reads this node's answer.
///
/// When stdin is not an interactive terminal, nothing is read at all: this
/// node's output is `node.default` if set, else an error. A workflow is
/// often run non-interactively (CI, piped input, another program driving
/// `lait run`), where there is no one to answer and no way to tell a closed
/// pipe from a slow human — attempting a read there would either fail
/// immediately with an unhelpful EOF error or hang forever, so `ask`
/// deliberately never tries.
///
/// When stdin *is* a terminal, the rendered `prompt` (and `choices`, if set)
/// are printed to stderr — like every other workflow progress line, so
/// piping this node's eventual answer/output on stdout stays clean — and one
/// line (or, with `node.multiline`, everything up to EOF) is read from it.
/// The read runs through `async_io::run_blocking` on a dedicated OS thread,
/// the same cancellation-aware admission point every other blocking I/O in
/// this codebase uses, so `timeout:`/a future SIGINT handler can still give
/// up on a stuck read (bounded by `async_io`'s own cleanup timeout) even
/// though the underlying blocking stdin read itself cannot be interrupted
/// mid-syscall.
pub(crate) async fn run_ask(
    prompt: &str,
    node: &AskNode,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        let default = node.default.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "stdin is not an interactive terminal and no 'default:' is set; an 'ask' node \
                 has no way to get an answer"
            )
        })?;
        // `default:` is still checked against `choices:` — it stands in for
        // a real answer, so it should be held to the same restriction one
        // would have been.
        return validate_choice(default, node.choices.as_deref());
    }

    eprintln!("{prompt}");
    if let Some(choices) = &node.choices {
        eprintln!("choices: {}", choices.join(", "));
    }

    let multiline = node.multiline.unwrap_or(false);
    let answer =
        async_io::run_blocking(move |_cancelled| read_answer(multiline), cancellation).await?;
    validate_choice(answer, node.choices.as_deref())
}

/// Reads one line (or, with `multiline`, everything up to EOF) from stdin,
/// stripping exactly one trailing CRLF/LF the same way a `command` node's
/// captured stdout does — a human's typed newline is not part of the answer.
fn read_answer(multiline: bool) -> Result<String> {
    let stdin = std::io::stdin();
    let mut buffer = String::new();
    if multiline {
        stdin
            .lock()
            .read_to_string(&mut buffer)
            .context("failed to read from stdin")?;
    } else {
        stdin
            .lock()
            .read_line(&mut buffer)
            .context("failed to read from stdin")?;
    }
    Ok(buffer.trim_end_matches(['\n', '\r']).to_owned())
}

/// Checks `answer` against `choices` (an exact match, after `read_answer`'s
/// own trailing-newline stripping — no further trimming): `None` means any
/// answer is accepted. A non-matching answer is a runtime error rather than
/// a re-prompt loop — stdin may not actually be interactive in every sense
/// (a script feeding fixed input through a pty, say), so looping on a bad
/// answer risks hanging instead of ever finishing.
fn validate_choice(answer: String, choices: Option<&[String]>) -> Result<String> {
    match choices {
        None => Ok(answer),
        Some(choices) if choices.iter().any(|choice| choice == &answer) => Ok(answer),
        Some(choices) => bail!(
            "answer {answer:?} is not one of the configured 'choices': {}",
            choices.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_choice;

    #[test]
    fn validate_choice_accepts_any_answer_when_no_choices_are_configured() {
        assert_eq!(
            validate_choice("anything".to_owned(), None).unwrap(),
            "anything"
        );
    }

    #[test]
    fn validate_choice_accepts_an_exact_match() {
        let choices = ["yes".to_owned(), "no".to_owned()];
        assert_eq!(
            validate_choice("yes".to_owned(), Some(&choices)).unwrap(),
            "yes"
        );
    }

    #[test]
    fn validate_choice_rejects_a_non_matching_answer() {
        let choices = ["yes".to_owned(), "no".to_owned()];
        let error = validate_choice("maybe".to_owned(), Some(&choices)).unwrap_err();
        assert!(error.to_string().contains("yes"));
        assert!(error.to_string().contains("no"));
    }

    #[test]
    fn validate_choice_is_case_sensitive() {
        let choices = ["Yes".to_owned()];
        assert!(validate_choice("yes".to_owned(), Some(&choices)).is_err());
    }
}
