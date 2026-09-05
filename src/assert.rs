//! Shared `assert:` evaluation for `lait test`'s and `lait eval`'s test
//! definition YAML (see docs/usage/ja/testing.md and docs/usage/ja/eval.md):
//! an `equals` (exact string match), `contains` (substring match), `jq`
//! (boolean jq expression), or `llm_judge` (LLM-as-judge scoring) check
//! against a workflow/model's final output text. `lait test` never passes an
//! [`LlmJudgeContext`] (it is replay-only and makes no model calls), so an
//! `llm_judge` assertion there always fails with a clear "not supported"
//! message rather than silently skipping it; `lait eval` always passes one.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::{
    config::{ConfigFile, ModelMap},
    engine::{
        AppContext, CapabilityOverrides, PromptTurn, SamplingOverrides, resolve_request_settings,
    },
    jq, response, schema,
};

/// The `llm_judge` pass/fail threshold when an assertion doesn't set its own
/// `threshold:`.
const DEFAULT_LLM_JUDGE_THRESHOLD: f64 = 0.7;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Assertion {
    /// The output must equal `value` exactly.
    Equals { value: String },
    /// The output must contain `value` as a substring (plain text, not
    /// JSON-normalized — unlike [`Assertion::Jq`]).
    Contains { value: String },
    /// `expr` must evaluate to a truthy jq value (jq's own truthiness rules:
    /// anything but `false`/`null`) against the output — see
    /// [`normalize_jq_input`] for how a plain-text output is exposed to it.
    Jq { expr: String },
    /// An LLM judges whether the output satisfies `criteria`, scoring it
    /// from 0.0 to 1.0; the assertion passes when the score is at least
    /// `threshold` (default [`DEFAULT_LLM_JUDGE_THRESHOLD`]). `model`
    /// defaults to the evaluation context's own default model (see
    /// [`LlmJudgeContext::default_model`]) when unset.
    LlmJudge {
        criteria: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        threshold: Option<f64>,
    },
}

/// One failed [`Assertion`], identified by its position in the original
/// `assert:` list (1-based, for display) alongside a human-readable reason.
pub(crate) struct AssertionFailure {
    pub(crate) position: usize,
    pub(crate) message: String,
}

/// The model-calling context an `llm_judge` assertion needs to actually call
/// a judge model — passed by `lait eval` (which always has a live model
/// connection). `lait test` passes `None` to [`evaluate`] instead (it is
/// replay-only and never calls a model), so an `llm_judge` assertion there
/// always fails with a "not supported" message.
pub(crate) struct LlmJudgeContext<'a> {
    pub(crate) env: &'a AppContext,
    pub(crate) file_config: &'a ConfigFile,
    /// The model an `llm_judge` assertion calls when it doesn't set its own
    /// `model:` — typically the eval target's own model, when it has one.
    pub(crate) default_model: Option<&'a str>,
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

/// The JSON Schema an `llm_judge` call requests as its Structured Output, so
/// the score/reasoning are always parseable rather than scraped from free
/// text.
fn llm_judge_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "score": {
                "type": "number",
                "description": "0.0 (does not satisfy the criteria at all) to 1.0 (fully satisfies it)",
            },
            "reasoning": {"type": "string"},
        },
        "required": ["score", "reasoning"],
        "additionalProperties": false,
    })
}

#[derive(Debug, Deserialize)]
struct JudgeScore {
    score: f64,
}

/// Calls a judge model to score `output` (and, when available, the `input`
/// that produced it) against `criteria`, returning the parsed score.
async fn run_llm_judge(
    judge: &LlmJudgeContext<'_>,
    criteria: &str,
    input: Option<&str>,
    output: &str,
    model_override: Option<&str>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<f64> {
    let model_name = model_override
        .or(judge.default_model)
        .ok_or_else(|| {
            anyhow!(
                "llm_judge requires a `model:` (no default model is available in this eval context)"
            )
        })?
        .to_owned();

    let settings = resolve_request_settings(
        model_name,
        SamplingOverrides::default(),
        None,
        None,
        CapabilityOverrides::default(),
        &ModelMap::default(),
        judge.file_config,
    )?
    .with_usage_label("llm_judge");

    let response_format = schema::build_json_schema(llm_judge_schema(), "llm_judge_score")?;

    let input_section = input
        .map(|input| format!("Original input:\n{input}\n\n"))
        .unwrap_or_default();
    let prompt = format!(
        "You are grading a language model's output against a criterion.\n\n\
         Criteria: {criteria}\n\n\
         {input_section}Output to grade:\n{output}\n\n\
         Rate how well the output satisfies the criteria, from 0.0 (not at all) to 1.0 (fully)."
    );

    let response = settings
        .complete(
            judge.env,
            &[],
            PromptTurn::simple(None, &prompt),
            Some(response_format),
            cancellation,
        )
        .await?;
    let content = response::content_text(&response);
    let parsed: JudgeScore = serde_json::from_str(content)
        .with_context(|| format!("failed to parse llm_judge response as JSON: {content:?}"))?;
    Ok(parsed.score)
}

/// Evaluates every entry in `assertions` against `output` (produced from
/// `input`, when the caller has one — only used by `llm_judge`) in order,
/// returning one [`AssertionFailure`] per entry that didn't hold (empty when
/// every assertion passed). A `jq` expression's own evaluation error (bad
/// syntax, a filter that doesn't produce exactly one value) and an
/// `llm_judge` call's own failure (no judge context, a model error, an
/// unparseable score) are both reported as a failure of that assertion
/// rather than aborting the rest.
pub(crate) async fn evaluate(
    assertions: &[Assertion],
    input: Option<&str>,
    output: &str,
    judge: Option<&LlmJudgeContext<'_>>,
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
            Assertion::Contains { value } => {
                if !output.contains(value.as_str()) {
                    failures.push(AssertionFailure {
                        position,
                        message: format!("expected output to contain {value:?}, got {output:?}"),
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
            Assertion::LlmJudge {
                criteria,
                model,
                threshold,
            } => match judge {
                None => failures.push(AssertionFailure {
                    position,
                    message: "llm_judge assertions are not supported here (no judge model \
                              available in this context; use `lait eval`)"
                        .to_owned(),
                }),
                Some(judge_context) => {
                    let threshold = threshold.unwrap_or(DEFAULT_LLM_JUDGE_THRESHOLD);
                    match run_llm_judge(
                        judge_context,
                        criteria,
                        input,
                        output,
                        model.as_deref(),
                        cancellation.clone(),
                    )
                    .await
                    {
                        Ok(score) if score >= threshold => {}
                        Ok(score) => failures.push(AssertionFailure {
                            position,
                            message: format!(
                                "llm_judge score {score:.2} is below threshold {threshold:.2} \
                                 for criteria {criteria:?}"
                            ),
                        }),
                        Err(error) => failures.push(AssertionFailure {
                            position,
                            message: format!("llm_judge evaluation failed: {error:#}"),
                        }),
                    }
                }
            },
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
        assert!(
            evaluate(&assertions, None, "hello", None, None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn equals_fails_on_a_mismatch() {
        let assertions = vec![Assertion::Equals {
            value: "hello".to_owned(),
        }];
        let failures = evaluate(&assertions, None, "goodbye", None, None).await;
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].position, 1);
    }

    #[tokio::test]
    async fn contains_passes_on_a_substring_match() {
        let assertions = vec![Assertion::Contains {
            value: "結論".to_owned(),
        }];
        assert!(
            evaluate(&assertions, None, "これは結論です", None, None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn contains_fails_when_the_substring_is_absent() {
        let assertions = vec![Assertion::Contains {
            value: "結論".to_owned(),
        }];
        let failures = evaluate(&assertions, None, "まだ途中です", None, None).await;
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn jq_passes_when_the_expression_is_truthy() {
        let assertions = vec![Assertion::Jq {
            expr: "contains(\"結論\")".to_owned(),
        }];
        assert!(
            evaluate(&assertions, None, "これは結論です", None, None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn jq_fails_when_the_expression_is_falsy() {
        let assertions = vec![Assertion::Jq {
            expr: "contains(\"結論\")".to_owned(),
        }];
        let failures = evaluate(&assertions, None, "まだ途中です", None, None).await;
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn jq_supports_structured_output_via_json_passthrough() {
        let assertions = vec![Assertion::Jq {
            expr: ".title | length > 0".to_owned(),
        }];
        assert!(
            evaluate(&assertions, None, r#"{"title": "hello"}"#, None, None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn jq_reports_a_syntax_error_as_a_failure_rather_than_panicking() {
        let assertions = vec![Assertion::Jq {
            expr: "not valid jq (((".to_owned(),
        }];
        let failures = evaluate(&assertions, None, "anything", None, None).await;
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
        let failures = evaluate(&assertions, None, "actual", None, None).await;
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].position, 1);
        assert_eq!(failures[1].position, 2);
    }

    #[tokio::test]
    async fn llm_judge_fails_clearly_without_a_judge_context() {
        let assertions = vec![Assertion::LlmJudge {
            criteria: "is it good?".to_owned(),
            model: None,
            threshold: None,
        }];
        let failures = evaluate(&assertions, None, "anything", None, None).await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("not supported"));
    }
}
