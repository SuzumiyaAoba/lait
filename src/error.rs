//! Process exit policy and typed interruptions shared by execution boundaries.

use std::fmt;

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
/// crate should never silently change a user's exit code. `is_lint` (set by
/// `main` from `Command::Lint` before `cli.command` is moved — see
/// `app.rs`'s module doc on why that classification is currently
/// duplicated) forces `Validation` unconditionally: `lait lint` reports
/// every issue as part of its normal output, so *any* error reaching this
/// far means the run itself failed to validate cleanly, not that something
/// crashed.
pub(crate) fn classify(error: &anyhow::Error, is_lint: bool) -> ExitKind {
    if is_lint {
        return ExitKind::Validation;
    }
    if error.downcast_ref::<Interrupted>().is_some() {
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
            assert_eq!(classify(&error, false), ExitKind::Interrupted);
            assert_eq!(classify(&error, true), ExitKind::Validation);
        }
    }

    #[test]
    fn incidental_words_do_not_classify_an_error_as_an_interruption() {
        for message in ["cannot open cancelled.yml", "server says: timed out"] {
            assert_eq!(
                classify(&anyhow::anyhow!(message), false),
                ExitKind::General
            );
        }
    }

    #[test]
    fn typed_context_preserves_interruption_policy_and_underlying_cause() {
        let error =
            anyhow::anyhow!("cleanup failed").context(Interrupted::cancelled("command stopped"));
        assert_eq!(classify(&error, false), ExitKind::Interrupted);
        assert!(format!("{error:#}").contains("cleanup failed"));
    }

    #[test]
    fn yaml_and_api_errors_keep_their_categories() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>("[").unwrap_err();
        assert_eq!(
            classify(&anyhow::Error::new(yaml).context("file"), false),
            ExitKind::Validation
        );
        let api = OpenAIError::StreamError(Box::new(
            async_openai::error::StreamError::EventStream("cancelled by upstream".into()),
        ));
        assert_eq!(
            classify(&anyhow::Error::new(api), false),
            ExitKind::ModelApi
        );
    }
}
