//! Shared `assert:` evaluation for `lait test`'s test definition YAML (see
//! docs/usage/ja/testing.md), sharing its vocabulary with the `assert:` an
//! upcoming `lait eval` (not implemented yet) is expected to reuse: an
//! `equals` (exact string match) or `jq` (boolean jq expression) check
//! against a workflow/model's final output text.

use serde::Deserialize;

use crate::jq;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Assertion {
    /// The output must equal `value` exactly.
    Equals { value: String },
    /// `expr` must evaluate to a truthy jq value (jq's own truthiness rules:
    /// anything but `false`/`null`) against the output — see
    /// [`normalize_jq_input`] for how a plain-text output is exposed to it.
    Jq { expr: String },
}

/// One failed [`Assertion`], identified by its position in the original
/// `assert:` list (1-based, for display) alongside a human-readable reason.
pub(crate) struct AssertionFailure {
    pub(crate) position: usize,
    pub(crate) message: String,
}

/// Converts a workflow/model's final output text into the JSON text a jq
/// expression evaluates `.` against: text that already parses as JSON is
/// passed through unchanged (so a structured assertion like
/// `.title | length > 0` works against a JSON-producing workflow), anything
/// else is wrapped as a JSON string value (so a plain-text assertion like
/// `contains("結論")` works against ordinary text output too).
pub(crate) fn normalize_jq_input(output: &str) -> String {
    if serde_json::from_str::<serde_json::Value>(output).is_ok() {
        output.to_owned()
    } else {
        serde_json::to_string(&serde_json::Value::String(output.to_owned()))
            .expect("serializing a string to JSON cannot fail")
    }
}

/// Evaluates every entry in `assertions` against `output` in order, returning
/// one [`AssertionFailure`] per entry that didn't hold (empty when every
/// assertion passed). A `jq` expression's own evaluation error (bad syntax, a
/// filter that doesn't produce exactly one value) is reported as a failure of
/// that assertion rather than aborting the rest.
pub(crate) async fn evaluate(
    assertions: &[Assertion],
    output: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Vec<AssertionFailure> {
    let input_json = normalize_jq_input(output);
    let empty_steps = jq::Steps::new();
    let mut failures = Vec::new();
    for (index, assertion) in assertions.iter().enumerate() {
        let position = index + 1;
        match assertion {
            Assertion::Equals { value } => {
                if output != value {
                    failures.push(AssertionFailure {
                        position,
                        message: format!("expected output to equal {value:?}, got {output:?}"),
                    });
                }
            }
            Assertion::Jq { expr } => {
                match jq::apply_bool_cancellable_async(
                    expr,
                    &input_json,
                    &empty_steps,
                    &empty_steps,
                    cancellation.clone(),
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => failures.push(AssertionFailure {
                        position,
                        message: format!("jq expression `{expr}` was false for output {output:?}"),
                    }),
                    Err(error) => failures.push(AssertionFailure {
                        position,
                        message: format!("jq expression `{expr}` failed: {error:#}"),
                    }),
                }
            }
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::{Assertion, evaluate, normalize_jq_input};

    #[test]
    fn normalizes_plain_text_as_a_json_string() {
        assert_eq!(normalize_jq_input("hello world"), "\"hello world\"");
    }

    #[test]
    fn passes_valid_json_text_through_unchanged() {
        assert_eq!(normalize_jq_input(r#"{"a":1}"#), r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn equals_passes_on_an_exact_match() {
        let assertions = vec![Assertion::Equals {
            value: "hello".to_owned(),
        }];
        assert!(evaluate(&assertions, "hello", None).await.is_empty());
    }

    #[tokio::test]
    async fn equals_fails_on_a_mismatch() {
        let assertions = vec![Assertion::Equals {
            value: "hello".to_owned(),
        }];
        let failures = evaluate(&assertions, "goodbye", None).await;
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].position, 1);
    }

    #[tokio::test]
    async fn jq_passes_when_the_expression_is_truthy() {
        let assertions = vec![Assertion::Jq {
            expr: "contains(\"結論\")".to_owned(),
        }];
        assert!(
            evaluate(&assertions, "これは結論です", None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn jq_fails_when_the_expression_is_falsy() {
        let assertions = vec![Assertion::Jq {
            expr: "contains(\"結論\")".to_owned(),
        }];
        let failures = evaluate(&assertions, "まだ途中です", None).await;
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn jq_supports_structured_output_via_json_passthrough() {
        let assertions = vec![Assertion::Jq {
            expr: ".title | length > 0".to_owned(),
        }];
        assert!(
            evaluate(&assertions, r#"{"title": "hello"}"#, None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn jq_reports_a_syntax_error_as_a_failure_rather_than_panicking() {
        let assertions = vec![Assertion::Jq {
            expr: "not valid jq (((".to_owned(),
        }];
        let failures = evaluate(&assertions, "anything", None).await;
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn reports_multiple_failures_with_their_original_position() {
        let assertions = vec![
            Assertion::Equals {
                value: "expected".to_owned(),
            },
            Assertion::Jq {
                expr: "contains(\"never\")".to_owned(),
            },
        ];
        let failures = evaluate(&assertions, "actual", None).await;
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].position, 1);
        assert_eq!(failures[1].position, 2);
    }
}
