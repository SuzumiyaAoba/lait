//! Shared `assert:` evaluation for `lait test`'s and `lait eval`'s test
//! definition YAML (see docs/usage/ja/testing.md and docs/usage/ja/eval.md):
//! an `equals` (exact string match), `contains` (substring match), `jq`
//! (boolean jq expression), or `llm_judge` (LLM-as-judge scoring) check
//! against a workflow/model's final output text. `lait test` never passes an
//! [`LlmJudgeContext`] (it is replay-only and makes no model calls), so an
//! `llm_judge` assertion there always fails with a clear "not supported"
//! message rather than silently skipping it; `lait eval` always passes one.

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::{
    config::{ConfigFile, ModelMap},
    engine::{
        CapabilityOverrides, EndpointOverrides, PromptTurn, RunContext, SamplingOverrides,
        resolve_request_settings,
    },
    jq, response, schema,
};

/// The `llm_judge` pass/fail threshold when an assertion doesn't set its own
/// `threshold:`.
const DEFAULT_LLM_JUDGE_THRESHOLD: f64 = 0.7;

/// The concurrency cap applied to independent assertion checks in
/// [`evaluate`]. Deliberately kept low (rather than mirroring
/// `engine::tool_loop::MAX_CONCURRENT_TOOL_CALLS`'s 8): `lait eval` already
/// runs up to `eval::EVAL_CONCURRENCY` (8) cases/repeats concurrently, each
/// of which can call [`evaluate`] independently, so the two caps multiply —
/// a case with several `llm_judge` assertions could otherwise drive up to
/// `EVAL_CONCURRENCY * MAX_CONCURRENT_ASSERTIONS` simultaneous model
/// requests. At 4 the worst case is 32, a bounded and modest increase over
/// the pre-parallelization baseline of ~8 (one in-flight model call per
/// case, since assertions used to run one at a time), while still letting a
/// case with 2-4 `llm_judge` assertions get most of the speedup.
const MAX_CONCURRENT_ASSERTIONS: usize = 4;

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
    pub(crate) env: &'a RunContext,
    pub(crate) file_config: &'a ConfigFile,
    /// The model an `llm_judge` assertion calls when it doesn't set its own
    /// `model:` — typically the eval target's own model, when it has one.
    pub(crate) default_model: Option<&'a str>,
    /// The input that produced `output`, shown to the judge model alongside
    /// it — `lait eval`'s own case input.
    pub(crate) input: Option<&'a str>,
}

/// Converts a workflow/model's final output text into the JSON text a jq
/// expression evaluates `.` against: text that already parses as JSON is
/// passed through unchanged (so a structured assertion like
/// `.title | length > 0` works against a JSON-producing workflow), anything
/// else is wrapped as a JSON string value (so a plain-text assertion like
/// `contains("結論")` works against ordinary text output too).
fn normalize_jq_input(output: &str) -> String {
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
        EndpointOverrides::default(),
        CapabilityOverrides::default(),
        &ModelMap::default(),
        judge.file_config,
    )?
    .with_usage_label("llm_judge");

    let response_format = schema::build_json_schema(llm_judge_schema(), "llm_judge_score")?;

    let input_section = judge
        .input
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

/// Checks [`Assertion::Equals`], returning a failure message when `output`
/// doesn't match `value` exactly.
fn check_equals(value: &str, output: &str) -> Option<String> {
    if output == value {
        None
    } else {
        Some(format!(
            "expected output to equal {value:?}, got {output:?}"
        ))
    }
}

/// Checks [`Assertion::Contains`], returning a failure message when `output`
/// doesn't contain `value` as a plain-text substring.
fn check_contains(value: &str, output: &str) -> Option<String> {
    if output.contains(value) {
        None
    } else {
        Some(format!(
            "expected output to contain {value:?}, got {output:?}"
        ))
    }
}

/// Checks [`Assertion::Jq`], returning a failure message when the expression
/// evaluates to `false` or fails outright (bad syntax, a filter that doesn't
/// produce exactly one value).
async fn check_jq(
    expr: &str,
    input_json: &str,
    empty_steps: &jq::Steps,
    output: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Option<String> {
    match jq::apply_bool_cancellable_async(expr, input_json, empty_steps, empty_steps, cancellation)
        .await
    {
        Ok(true) => None,
        Ok(false) => Some(format!(
            "jq expression `{expr}` was false for output {output:?}"
        )),
        Err(error) => Some(format!("jq expression `{expr}` failed: {error:#}")),
    }
}

/// Checks [`Assertion::LlmJudge`], returning a failure message when no judge
/// context is available, the judge call itself fails, or the score falls
/// short of `threshold`.
async fn check_llm_judge(
    judge: Option<&LlmJudgeContext<'_>>,
    criteria: &str,
    model: Option<&str>,
    threshold: Option<f64>,
    output: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Option<String> {
    let Some(judge_context) = judge else {
        return Some(
            "llm_judge assertions are not supported here (no judge model \
             available in this context; use `lait eval`)"
                .to_owned(),
        );
    };
    let threshold = threshold.unwrap_or(DEFAULT_LLM_JUDGE_THRESHOLD);
    match run_llm_judge(judge_context, criteria, output, model, cancellation).await {
        Ok(score) if score >= threshold => None,
        Ok(score) => Some(format!(
            "llm_judge score {score:.2} is below threshold {threshold:.2} \
             for criteria {criteria:?}"
        )),
        Err(error) => Some(format!("llm_judge evaluation failed: {error:#}")),
    }
}

/// Evaluates every entry in `assertions` against `output`, returning one
/// [`AssertionFailure`] per entry that didn't hold, in `assertions`' original
/// order (empty when every assertion passed). A `jq` expression's own
/// evaluation error (bad syntax, a filter that doesn't produce exactly one
/// value) and an `llm_judge` call's own failure (no judge context, a model
/// error, an unparseable score) are both reported as a failure of that
/// assertion rather than aborting the rest.
///
/// Assertions are independent (no short-circuiting, no shared mutable
/// state), so they run with a fixed concurrency cap
/// ([`MAX_CONCURRENT_ASSERTIONS`]) rather than one at a time — an `llm_judge`
/// assertion is a full model round trip, and a test/eval case with several
/// of them used to pay for that serially. `buffered` (not
/// `buffer_unordered`) preserves `assertions`' order, so the `position` on
/// each result still lines up with its original 1-based index.
pub(crate) async fn evaluate(
    assertions: &[Assertion],
    judge: Option<&LlmJudgeContext<'_>>,
    output: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Vec<AssertionFailure> {
    let input_json = normalize_jq_input(output);
    let empty_steps = jq::Steps::new();
    let input_json = &input_json;
    let empty_steps = &empty_steps;

    let checks = assertions.iter().map(move |assertion| {
        let cancellation = cancellation.clone();
        async move {
            match assertion {
                Assertion::Equals { value } => check_equals(value, output),
                Assertion::Contains { value } => check_contains(value, output),
                Assertion::Jq { expr } => {
                    check_jq(expr, input_json, empty_steps, output, cancellation).await
                }
                Assertion::LlmJudge {
                    criteria,
                    model,
                    threshold,
                } => {
                    check_llm_judge(
                        judge,
                        criteria,
                        model.as_deref(),
                        *threshold,
                        output,
                        cancellation,
                    )
                    .await
                }
            }
        }
    });

    futures_util::stream::iter(checks)
        .buffered(MAX_CONCURRENT_ASSERTIONS)
        .collect::<Vec<Option<String>>>()
        .await
        .into_iter()
        .enumerate()
        .filter_map(|(index, message)| {
            message.map(|message| AssertionFailure {
                position: index + 1,
                message,
            })
        })
        .collect()
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
        assert!(evaluate(&assertions, None, "hello", None).await.is_empty());
    }

    #[tokio::test]
    async fn equals_fails_on_a_mismatch() {
        let assertions = vec![Assertion::Equals {
            value: "hello".to_owned(),
        }];
        let failures = evaluate(&assertions, None, "goodbye", None).await;
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].position, 1);
    }

    #[tokio::test]
    async fn contains_passes_on_a_substring_match() {
        let assertions = vec![Assertion::Contains {
            value: "結論".to_owned(),
        }];
        assert!(
            evaluate(&assertions, None, "これは結論です", None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn contains_fails_when_the_substring_is_absent() {
        let assertions = vec![Assertion::Contains {
            value: "結論".to_owned(),
        }];
        let failures = evaluate(&assertions, None, "まだ途中です", None).await;
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn jq_passes_when_the_expression_is_truthy() {
        let assertions = vec![Assertion::Jq {
            expr: "contains(\"結論\")".to_owned(),
        }];
        assert!(
            evaluate(&assertions, None, "これは結論です", None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn jq_fails_when_the_expression_is_falsy() {
        let assertions = vec![Assertion::Jq {
            expr: "contains(\"結論\")".to_owned(),
        }];
        let failures = evaluate(&assertions, None, "まだ途中です", None).await;
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn jq_supports_structured_output_via_json_passthrough() {
        let assertions = vec![Assertion::Jq {
            expr: ".title | length > 0".to_owned(),
        }];
        assert!(
            evaluate(&assertions, None, r#"{"title": "hello"}"#, None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn jq_reports_a_syntax_error_as_a_failure_rather_than_panicking() {
        let assertions = vec![Assertion::Jq {
            expr: "not valid jq (((".to_owned(),
        }];
        let failures = evaluate(&assertions, None, "anything", None).await;
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
        let failures = evaluate(&assertions, None, "actual", None).await;
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].position, 1);
        assert_eq!(failures[1].position, 2);
    }

    /// With 6 assertions and a concurrency cap of [`MAX_CONCURRENT_ASSERTIONS`]
    /// (4), this spans at least two `buffered` windows, so it would catch a
    /// regression to `buffer_unordered` (which does not preserve order) or
    /// any indexing mistake in how `position` is derived from the original
    /// list.
    #[tokio::test]
    async fn preserves_failure_order_and_count_across_multiple_buffered_windows() {
        let assertions = vec![
            Assertion::Equals {
                value: "no-match-1".to_owned(),
            },
            Assertion::Contains {
                value: "結論".to_owned(),
            },
            Assertion::Jq {
                expr: "contains(\"never\")".to_owned(),
            },
            Assertion::Equals {
                value: "actual".to_owned(),
            },
            Assertion::Contains {
                value: "no-match-2".to_owned(),
            },
            Assertion::Jq {
                expr: "contains(\"missing\")".to_owned(),
            },
        ];
        let failures = evaluate(&assertions, None, "actual", None).await;
        // Positions 1 (equals mismatch), 2 (contains mismatch), 3 (jq
        // false), 5 (contains mismatch), and 6 (jq false) fail; only 4
        // (equals an exact match) passes.
        assert_eq!(
            failures
                .iter()
                .map(|failure| failure.position)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 5, 6]
        );
    }

    #[tokio::test]
    async fn llm_judge_fails_clearly_without_a_judge_context() {
        let assertions = vec![Assertion::LlmJudge {
            criteria: "is it good?".to_owned(),
            model: None,
            threshold: None,
        }];
        let failures = evaluate(&assertions, None, "anything", None).await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("not supported"));
    }
}
