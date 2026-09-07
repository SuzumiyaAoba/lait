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

/// Clap owns usage errors (2); the signal handler owns SIGINT (130).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitKind {
    General = 1,
    Validation = 3,
    ModelApi = 4,
    Interrupted = 5,
}

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
