//! Process exit policy and typed interruptions shared by execution boundaries.

use std::fmt;

use anyhow::anyhow;
use async_openai::error::OpenAIError;

/// An intentional cancellation or an elapsed execution deadline.
/// Keep this in the error chain instead of deriving control flow from prose.
#[derive(Debug)]
pub(crate) enum Interrupted {
    Cancelled(String),
    TimedOut(String),
}

impl Interrupted {
    pub(crate) fn cancelled(message: impl Into<String>) -> Self {
        Self::Cancelled(message.into())
    }

    pub(crate) fn timed_out(message: impl Into<String>) -> Self {
        Self::TimedOut(message.into())
    }
}

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(message) | Self::TimedOut(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Interrupted {}

/// Builds a ready-to-return `anyhow::Error` wrapping a cancellation
/// [`Interrupted`] — the one spelling every cancellation call site in the
/// crate should use. `Interrupted::cancelled` only returns `Self`, so
/// lifting it into the `anyhow::Error` almost every call site actually needs
/// used to be done six different ways across the crate (`bail!(Interrupted
/// ::cancelled(..))`, `Err(anyhow!(Interrupted::cancelled(..)))`,
/// `Err(anyhow::Error::new(..))`, `.into()`, a bare `Err(Interrupted::
/// cancelled(..))`, and a short-import `bail!(Interrupted::cancelled(..))`),
/// with the choice of import (`crate::error::Interrupted` fully qualified vs.
/// a local `use`) varying just as much. Prefer `bail!(error::cancelled(".."))`
/// at a call site that returns `Result` (or `return Err(error::cancelled(".."))`
/// where `bail!` isn't available, e.g. inside a `match`/`select!` arm).
pub(crate) fn cancelled(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Interrupted::cancelled(message))
}

/// The elapsed-deadline counterpart to [`cancelled`] — see its doc comment.
pub(crate) fn timed_out(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Interrupted::timed_out(message))
}

/// Shared by `workflow_run::run_workflow` and `compare::run`: both resolve
/// `PROMPT` the same way (`chat::resolve_input_with_stdin_cancellable`) and
/// fail identically when neither a positional argument nor piped stdin
/// supplied one. Lives here (rather than on either call site's own module)
/// so `compare` doesn't need to depend on `app` just for this one error —
/// that single-function edge used to be `compare`'s only reason to import
/// from `app`, forming a needless `{app, compare}` dependency cycle.
pub(crate) fn missing_prompt_error() -> anyhow::Error {
    anyhow!("a PROMPT is required; provide one or pipe input via stdin")
}

/// Whether `error` carries an [`Interrupted`] anywhere in it — the read-side
/// counterpart to [`cancelled`]/[`timed_out`]'s write side. Before this
/// existed, call sites that needed to tell a genuine cancellation apart from
/// an ordinary failure each spelled out their own
/// `error.downcast_ref::<Interrupted>().is_some()` or, less reliably,
/// `error.chain().any(|cause| cause.is::<Interrupted>())`.
///
/// Deliberately `downcast_ref`, not a manual `.chain().any(..)` walk: when
/// `Interrupted` is attached via `.context(..)` rather than being the error
/// itself (`process::run_process`'s cleanup-failure arm does this so the
/// cleanup failure stays in the chain too — see its comment), anyhow's
/// `downcast_ref` still finds it because anyhow searches context values as
/// well as `source()` links, but a hand-rolled `.chain()` walk only sees
/// `source()` links and misses it. This is exactly `classify`'s existing
/// check, pulled out so every other call site gets it right too.
pub(crate) fn is_interrupted(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Interrupted>().is_some()
}

/// Clap owns usage errors (2); the signal handler owns SIGINT (130).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitKind {
    General = 1,
    Validation = 3,
    ModelApi = 4,
    Interrupted = 5,
}

/// The single entry point deciding a failed invocation's process exit code
/// (`main::exit_with_error` calls this and casts the result to `i32`).
/// Deliberately classifies by error *type* via `downcast_ref`/`chain().any`
/// rather than by matching message text — a wording change elsewhere in the
/// crate should never silently change a user's exit code. `forced`, when
/// `Some`, skips that classification entirely and returns the given kind —
/// `main` passes `Some(ExitKind::Validation)` for `Command::Lint` (decided
/// from `dispatch` before `cli.command` is moved), since `lait lint` reports
/// every issue as part of its normal output, so *any* error reaching this far
/// means the run itself failed to validate cleanly, not that something
/// crashed. Spelling this as `Option<ExitKind>` rather than a bare
/// `is_lint: bool` makes "lint always forces Validation" explicit at the
/// call site instead of requiring this doc comment to explain what the flag
/// does.
pub(crate) fn classify(error: &anyhow::Error, forced: Option<ExitKind>) -> ExitKind {
    if let Some(forced) = forced {
        return forced;
    }
    if is_interrupted(error) {
        return ExitKind::Interrupted;
    }
    if error.chain().any(|cause| cause.is::<OpenAIError>()) {
        return ExitKind::ModelApi;
    }
    if error.chain().any(|cause| cause.is::<serde_yaml::Error>()) {
        return ExitKind::Validation;
    }
    ExitKind::General
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_interruptions_through_context_without_matching_words() {
        for interruption in [
            Interrupted::cancelled("停止"),
            Interrupted::timed_out("期限"),
        ] {
            let error = anyhow::Error::new(interruption).context("step failed");
            assert_eq!(classify(&error, None), ExitKind::Interrupted);
            assert_eq!(
                classify(&error, Some(ExitKind::Validation)),
                ExitKind::Validation
            );
        }
    }

    #[test]
    fn incidental_words_do_not_classify_an_error_as_an_interruption() {
        for message in ["cannot open cancelled.yml", "server says: timed out"] {
            assert_eq!(classify(&anyhow::anyhow!(message), None), ExitKind::General);
        }
    }

    #[test]
    fn is_interrupted_finds_the_marker_anywhere_in_the_chain() {
        let bare = anyhow::Error::new(Interrupted::cancelled("停止"));
        assert!(is_interrupted(&bare));

        let wrapped = anyhow::anyhow!("cleanup failed").context(Interrupted::timed_out("期限"));
        assert!(is_interrupted(&wrapped));

        let unrelated = anyhow::anyhow!("cannot open cancelled.yml");
        assert!(!is_interrupted(&unrelated));
    }

    #[test]
    fn typed_context_preserves_interruption_policy_and_underlying_cause() {
        let error =
            anyhow::anyhow!("cleanup failed").context(Interrupted::cancelled("command stopped"));
        assert_eq!(classify(&error, None), ExitKind::Interrupted);
        assert!(format!("{error:#}").contains("cleanup failed"));
    }

    #[test]
    fn yaml_and_api_errors_keep_their_categories() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>("[").unwrap_err();
        assert_eq!(
            classify(&anyhow::Error::new(yaml).context("file"), None),
            ExitKind::Validation
        );
        let api = OpenAIError::StreamError(Box::new(
            async_openai::error::StreamError::EventStream("cancelled by upstream".into()),
        ));
        assert_eq!(classify(&anyhow::Error::new(api), None), ExitKind::ModelApi);
    }
}
