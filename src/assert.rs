//! Shared `assert:` evaluation for `lait test`'s and `lait eval`'s test
//! definition YAML (see docs/usage/ja/testing.md and docs/usage/ja/eval.md):
//! an `equals` (exact string match), `contains` (substring match), `jq`
//! (boolean jq expression), `llm_judge` (LLM-as-judge scoring), or a
//! trajectory check (`tool_called`/`usage`/`step_output`) against a
//! workflow/model's final output text and its recorded execution trace.
//! `lait test` never passes an [`LlmJudgeContext`] (it is replay-only and
//! makes no model calls), so an `llm_judge` assertion there always fails
//! with a clear "not supported" message rather than silently skipping it.
//! Both `lait test` and `lait eval` always pass a [`TrajectoryContext`]:
//! `lait test` runs its target workflow through a `RunContext` owned by
//! that one test file, and `lait eval`'s `run_case` gives every (case,
//! repeat) run its own fresh `RunContext` (sharing only the read-only
//! `AppServices` registries/caches) specifically so a trajectory
//! assertion's events/usage are never polluted by another concurrently
//! running case — see `eval::run_case`'s doc comment.

use std::{future::Future, pin::Pin};

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::{
    config::{ConfigFile, ModelMap},
    engine::{
        CapabilityOverrides, EndpointOverrides, PromptTurn, RunContext, SamplingOverrides,
        resolve_request_settings,
    },
    jq, response, schema, trace, workflow,
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
    /// A tool matching the qualified name `name` (e.g. `tool__ripgrep`,
    /// `mcp__server__tool`, `agent__researcher`) was called at least `min`
    /// (default 1) and at most `max` (default unbounded) times during the
    /// run, and — when `args_jq` is set — at least one matching call's
    /// arguments satisfied it (evaluated the same way [`Assertion::Jq`]
    /// evaluates against output: the raw JSON arguments text as `.`).
    /// Requires a [`TrajectoryContext`] — see this module's doc comment.
    ToolCalled {
        name: String,
        #[serde(default)]
        min: Option<u64>,
        #[serde(default)]
        max: Option<u64>,
        #[serde(default)]
        args_jq: Option<String>,
    },
    /// The run's total token usage/cost (summed across every model call the
    /// run made, the same totals `--show-usage` prints) stayed within every
    /// bound given (any omitted bound is unchecked). `max_cost_usd` only
    /// ever checks a run where at least one resolved model has `pricing:`
    /// configured — see `TrajectoryContext::cost_total`'s doc comment; when
    /// nothing was priced, `max_cost_usd` is simply not checked (cost is
    /// unknown, not zero). Requires a [`TrajectoryContext`].
    Usage {
        #[serde(default)]
        max_prompt_tokens: Option<u64>,
        #[serde(default)]
        max_completion_tokens: Option<u64>,
        #[serde(default)]
        max_total_tokens: Option<u64>,
        #[serde(default)]
        max_cost_usd: Option<f64>,
    },
    /// Every assertion in `assert` holds against the named workflow step's
    /// own output (rather than the run's final output). A trajectory
    /// assertion (`tool_called`/`usage`/`step_output`) nested here still
    /// evaluates against the *whole run*, not scoped to just this step —
    /// recorded tool-call events are not currently attributed to a single
    /// step narrowly enough to support that. Requires a
    /// [`TrajectoryContext`].
    StepOutput { id: String, assert: Vec<Assertion> },
}

/// One failed [`Assertion`], identified by its position in the original
/// `assert:` list (1-based, for display) alongside a human-readable reason.
#[derive(Debug)]
pub(crate) struct AssertionFailure {
    pub(crate) position: usize,
    pub(crate) message: String,
}

/// The `assertion {position}: {message}` report line `lait test`/`lait eval`
/// spell per failed assertion — one impl so the two reports can't drift.
impl std::fmt::Display for AssertionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "assertion {}: {}", self.position, self.message)
    }
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

/// The per-run execution data a trajectory assertion
/// (`tool_called`/`usage`/`step_output`) needs — passed by `lait test`
/// (`test_run::run_test_file_inner`) and `lait eval` (`eval::run_case`),
/// both of which give the run producing `events`/`usage_total` its own
/// `RunContext`, so this is unambiguously "this run's, and only this run's"
/// data — see this module's doc comment. `None` when a caller genuinely has
/// no such context to offer (`evaluate`'s only other caller today is
/// `test_run`/`eval` themselves, so this is currently more a documented
/// possibility than a live code path).
pub(crate) struct TrajectoryContext<'a> {
    /// Every model-call/tool-call event `crate::trace::TraceCollector`
    /// recorded during the run, in recording order (see
    /// `trace::TraceCollector::events`).
    pub(crate) events: &'a [trace::TraceEvent],
    /// The run's `$steps`/`{{ steps.<id> }}` map — the same value a workflow
    /// template/jq filter would see, letting `step_output` look up a named
    /// step's own output by id.
    pub(crate) steps_outputs: &'a workflow::StepOutputs,
    /// The run's total token usage, when the server reported any (see
    /// `usage::UsageTally::total`).
    pub(crate) usage_total: Option<response::Usage>,
    /// The run's total estimated USD cost, when at least one recorded model
    /// has `pricing:` configured (see `usage::UsageTally::total_cost`).
    pub(crate) cost_total: Option<f64>,
}

/// The message every trajectory assertion (`tool_called`/`usage`/
/// `step_output`) fails with when no [`TrajectoryContext`] is available —
/// see [`TrajectoryContext`]'s own doc comment for when that happens.
fn trajectory_unsupported_message() -> String {
    "trajectory assertions (tool_called/usage/step_output) are not \
     supported here (no per-run trace context is available in this context)"
        .to_owned()
}

/// The text a workflow step's own output renders as (see
/// `template::to_text`): a string output is its own text, any other value
/// its compact JSON — the same text form a later step, `lait run`'s final
/// output, or a written file sees.
fn step_output_text(value: &serde_json::Value) -> String {
    crate::template::to_text(value)
}

/// Checks [`Assertion::ToolCalled`]: `events` must contain between `min`
/// (default 1) and `max` (default unbounded) `"execute_tool"` events whose
/// `gen_ai.tool.name` attribute equals `name`, and — when `args_jq` is set —
/// at least one of those calls' recorded `gen_ai.tool.arguments` must
/// satisfy it.
async fn check_tool_called(
    events: &[trace::TraceEvent],
    name: &str,
    min: Option<u64>,
    max: Option<u64>,
    args_jq: Option<&str>,
    cancellation: tokio_util::sync::CancellationToken,
) -> Option<String> {
    let matching: Vec<&trace::TraceEvent> = events
        .iter()
        .filter(|event| {
            event.operation == "execute_tool"
                && event
                    .attributes
                    .get("gen_ai.tool.name")
                    .and_then(serde_json::Value::as_str)
                    == Some(name)
        })
        .collect();
    let count = matching.len() as u64;
    let min = min.unwrap_or(1);
    if count < min {
        return Some(format!(
            "expected tool '{name}' to be called at least {min} time(s), was called {count} time(s)"
        ));
    }
    if let Some(max) = max
        && count > max
    {
        return Some(format!(
            "expected tool '{name}' to be called at most {max} time(s), was called {count} time(s)"
        ));
    }
    let expr = args_jq?;
    let globals = jq::Globals::default();
    for event in &matching {
        let Some(arguments) = event
            .attributes
            .get("gen_ai.tool.arguments")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if let Ok(true) = jq::eval_bool_async(
            expr,
            &normalize_jq_input(arguments),
            &globals,
            cancellation.clone(),
        )
        .await
        {
            return None;
        }
    }
    Some(format!(
        "no call to tool '{name}' had arguments satisfying `{expr}`"
    ))
}

/// Checks [`Assertion::Usage`] against the run's total usage (zero in every
/// field when the server reported none at all — the same default
/// [`response::Usage`] uses elsewhere).
fn check_usage(
    usage_total: Option<response::Usage>,
    cost_total: Option<f64>,
    max_prompt_tokens: Option<u64>,
    max_completion_tokens: Option<u64>,
    max_total_tokens: Option<u64>,
    max_cost_usd: Option<f64>,
) -> Option<String> {
    let usage = usage_total.unwrap_or_default();
    if let Some(max) = max_prompt_tokens
        && usage.prompt_tokens > max
    {
        return Some(format!(
            "prompt_tokens {} exceeded max_prompt_tokens {max}",
            usage.prompt_tokens
        ));
    }
    if let Some(max) = max_completion_tokens
        && usage.completion_tokens > max
    {
        return Some(format!(
            "completion_tokens {} exceeded max_completion_tokens {max}",
            usage.completion_tokens
        ));
    }
    if let Some(max) = max_total_tokens
        && usage.total_tokens > max
    {
        return Some(format!(
            "total_tokens {} exceeded max_total_tokens {max}",
            usage.total_tokens
        ));
    }
    // Only checked when at least one recorded model was actually priced —
    // see `TrajectoryContext::cost_total`'s doc comment: `None` means "cost
    // unknown", not "cost zero", so this never fails a run just because
    // nothing it called had `pricing:` configured.
    if let Some(max) = max_cost_usd
        && let Some(cost) = cost_total
        && cost > max
    {
        return Some(format!(
            "cost {} exceeded max_cost_usd {}",
            crate::usage::format_cost(cost),
            crate::usage::format_cost(max)
        ));
    }
    None
}

/// Checks [`Assertion::StepOutput`]: looks up `id` in
/// `trajectory.steps_outputs`, then recursively [`evaluate`]s `nested`
/// against that step's own output text (see [`step_output_text`]). Every
/// nested failure is folded into one message (`step_output` is itself one
/// entry in the caller's `assert:` list, so it can only ever produce one
/// [`AssertionFailure`] of its own).
async fn check_step_output(
    id: &str,
    nested: &[Assertion],
    judge: Option<&LlmJudgeContext<'_>>,
    trajectory: Option<&TrajectoryContext<'_>>,
    cancellation: tokio_util::sync::CancellationToken,
) -> Option<String> {
    let Some(trajectory) = trajectory else {
        return Some(trajectory_unsupported_message());
    };
    let Some(value) = trajectory.steps_outputs.get(id) else {
        return Some(format!(
            "step '{id}' produced no output (it may not have run, or the id doesn't exist)"
        ));
    };
    let text = step_output_text(value);
    // Boxed and coerced to `dyn Future` (rather than a plain
    // `evaluate(..).await`) so this recursive call doesn't give `evaluate`'s
    // own future type an infinite size — see `evaluate`'s doc comment. Not
    // `+ Send`: nothing in this call chain (`evaluate`'s own
    // `futures_util::stream::buffered`, `lait test`'s per-file
    // `stream::buffered` over `run_test_file`) ever crosses a `tokio::spawn`
    // boundary — every one of them is plain cooperative polling within a
    // single task — so requiring `Send` here would only recreate the
    // Send-inference cycle this boxing is meant to avoid, for a bound
    // nothing downstream actually needs.
    let recurse: Pin<Box<dyn Future<Output = Vec<AssertionFailure>> + '_>> = Box::pin(evaluate(
        nested,
        judge,
        Some(trajectory),
        &text,
        cancellation,
    ));
    let failures = recurse.await;
    if failures.is_empty() {
        return None;
    }
    Some(format!(
        "step '{id}': {}",
        failures
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

/// Converts a workflow/model's final output text into the value a jq
/// expression evaluates `.` against: text that parses as JSON is used as
/// that value (so a structured assertion like `.title | length > 0` works
/// against a JSON-producing workflow), anything else is a JSON string (so a
/// plain-text assertion like `contains("結論")` works against ordinary text
/// output too).
fn normalize_jq_input(output: &str) -> serde_json::Value {
    crate::template::parse_input(output)
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
    cancellation: tokio_util::sync::CancellationToken,
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
    input_value: &serde_json::Value,
    globals: &jq::Globals,
    output: &str,
    cancellation: tokio_util::sync::CancellationToken,
) -> Option<String> {
    match jq::eval_bool_async(expr, input_value, globals, cancellation).await {
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
    cancellation: tokio_util::sync::CancellationToken,
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
///
/// [`check_step_output`] calls this function recursively for a nested
/// `assert:` list; it boxes that one recursive call (`Box::pin(evaluate(..))`
/// coerced to a `dyn Future`) rather than this function returning a boxed
/// future itself — the latter was tried first and rejected: it made the
/// `assertions.iter().map(move |assertion| async move { .. })` closure
/// below fail to type-check with "implementation of `FnOnce` is not general
/// enough", an unrelated higher-ranked-lifetime inference failure the
/// compiler's trait solver produces once this function's own return type
/// carries an explicit lifetime parameter tied to a `dyn Future` (see
/// `engine/transport.rs`'s similar `AsyncFn`/HRTB doc comment for another
/// instance of this same class of trait-solver limitation). Boxing only at
/// the recursive call site avoids ever giving `evaluate` itself a
/// non-elided return lifetime.
pub(crate) async fn evaluate(
    assertions: &[Assertion],
    judge: Option<&LlmJudgeContext<'_>>,
    trajectory: Option<&TrajectoryContext<'_>>,
    output: &str,
    cancellation: tokio_util::sync::CancellationToken,
) -> Vec<AssertionFailure> {
    let input_value = normalize_jq_input(output);
    let globals = jq::Globals::default();
    let input_value = &input_value;
    let globals = &globals;

    let checks = assertions.iter().map(move |assertion| {
        let cancellation = cancellation.clone();
        async move {
            match assertion {
                Assertion::Equals { value } => check_equals(value, output),
                Assertion::Contains { value } => check_contains(value, output),
                Assertion::Jq { expr } => {
                    check_jq(expr, input_value, globals, output, cancellation).await
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
                Assertion::ToolCalled {
                    name,
                    min,
                    max,
                    args_jq,
                } => match trajectory {
                    Some(trajectory) => {
                        check_tool_called(
                            trajectory.events,
                            name,
                            *min,
                            *max,
                            args_jq.as_deref(),
                            cancellation,
                        )
                        .await
                    }
                    None => Some(trajectory_unsupported_message()),
                },
                Assertion::Usage {
                    max_prompt_tokens,
                    max_completion_tokens,
                    max_total_tokens,
                    max_cost_usd,
                } => match trajectory {
                    Some(trajectory) => check_usage(
                        trajectory.usage_total,
                        trajectory.cost_total,
                        *max_prompt_tokens,
                        *max_completion_tokens,
                        *max_total_tokens,
                        *max_cost_usd,
                    ),
                    None => Some(trajectory_unsupported_message()),
                },
                Assertion::StepOutput { id, assert } => {
                    check_step_output(id, assert, judge, trajectory, cancellation).await
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
    use super::{Assertion, TrajectoryContext, evaluate, normalize_jq_input, trace, workflow};

    #[test]
    fn normalizes_plain_text_as_a_json_string() {
        assert_eq!(
            normalize_jq_input("hello world"),
            serde_json::json!("hello world")
        );
    }

    #[test]
    fn passes_valid_json_text_through_unchanged() {
        assert_eq!(
            normalize_jq_input(r#"{"a":1}"#),
            serde_json::json!({"a": 1})
        );
    }

    #[tokio::test]
    async fn equals_passes_on_an_exact_match() {
        let assertions = vec![Assertion::Equals {
            value: "hello".to_owned(),
        }];
        assert!(
            evaluate(
                &assertions,
                None,
                None,
                "hello",
                crate::cancellation::none()
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn equals_fails_on_a_mismatch() {
        let assertions = vec![Assertion::Equals {
            value: "hello".to_owned(),
        }];
        let failures = evaluate(
            &assertions,
            None,
            None,
            "goodbye",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].position, 1);
    }

    #[tokio::test]
    async fn contains_passes_on_a_substring_match() {
        let assertions = vec![Assertion::Contains {
            value: "結論".to_owned(),
        }];
        assert!(
            evaluate(
                &assertions,
                None,
                None,
                "これは結論です",
                crate::cancellation::none()
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn contains_fails_when_the_substring_is_absent() {
        let assertions = vec![Assertion::Contains {
            value: "結論".to_owned(),
        }];
        let failures = evaluate(
            &assertions,
            None,
            None,
            "まだ途中です",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn jq_passes_when_the_expression_is_truthy() {
        let assertions = vec![Assertion::Jq {
            expr: "contains(\"結論\")".to_owned(),
        }];
        assert!(
            evaluate(
                &assertions,
                None,
                None,
                "これは結論です",
                crate::cancellation::none()
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn jq_fails_when_the_expression_is_falsy() {
        let assertions = vec![Assertion::Jq {
            expr: "contains(\"結論\")".to_owned(),
        }];
        let failures = evaluate(
            &assertions,
            None,
            None,
            "まだ途中です",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn jq_supports_structured_output_via_json_passthrough() {
        let assertions = vec![Assertion::Jq {
            expr: ".title | length > 0".to_owned(),
        }];
        assert!(
            evaluate(
                &assertions,
                None,
                None,
                r#"{"title": "hello"}"#,
                crate::cancellation::none()
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn jq_reports_a_syntax_error_as_a_failure_rather_than_panicking() {
        let assertions = vec![Assertion::Jq {
            expr: "not valid jq (((".to_owned(),
        }];
        let failures = evaluate(
            &assertions,
            None,
            None,
            "anything",
            crate::cancellation::none(),
        )
        .await;
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
        let failures = evaluate(
            &assertions,
            None,
            None,
            "actual",
            crate::cancellation::none(),
        )
        .await;
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
        let failures = evaluate(
            &assertions,
            None,
            None,
            "actual",
            crate::cancellation::none(),
        )
        .await;
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
        let failures = evaluate(
            &assertions,
            None,
            None,
            "anything",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("not supported"));
    }

    /// Builds an `"execute_tool"` event the way `engine::tool_loop` records
    /// one, with just the attributes `check_tool_called` reads.
    fn tool_event(name: &str, arguments: &str) -> trace::TraceEvent {
        let now = chrono::Utc::now();
        trace::TraceEvent {
            seq: 0,
            operation: "execute_tool".to_owned(),
            label: format!("tool '{name}'"),
            start: now,
            end: now,
            duration_ms: 0,
            attributes: trace::attrs([
                ("gen_ai.tool.name", serde_json::Value::from(name)),
                ("gen_ai.tool.arguments", serde_json::Value::from(arguments)),
            ]),
        }
    }

    #[tokio::test]
    async fn tool_called_passes_when_the_tool_was_called_at_least_once() {
        let events = vec![tool_event("tool__echo", "{}")];
        let steps_outputs = workflow::StepOutputs::new();
        let trajectory = TrajectoryContext {
            events: &events,
            steps_outputs: &steps_outputs,
            usage_total: None,
            cost_total: None,
        };
        let assertions = vec![Assertion::ToolCalled {
            name: "tool__echo".to_owned(),
            min: None,
            max: None,
            args_jq: None,
        }];
        let failures = evaluate(
            &assertions,
            None,
            Some(&trajectory),
            "output",
            crate::cancellation::none(),
        )
        .await;
        assert!(failures.is_empty(), "failures: {failures:?}");
    }

    #[tokio::test]
    async fn tool_called_fails_when_the_tool_was_never_called() {
        let events: Vec<trace::TraceEvent> = Vec::new();
        let steps_outputs = workflow::StepOutputs::new();
        let trajectory = TrajectoryContext {
            events: &events,
            steps_outputs: &steps_outputs,
            usage_total: None,
            cost_total: None,
        };
        let assertions = vec![Assertion::ToolCalled {
            name: "tool__echo".to_owned(),
            min: None,
            max: None,
            args_jq: None,
        }];
        let failures = evaluate(
            &assertions,
            None,
            Some(&trajectory),
            "output",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("at least"));
    }

    #[tokio::test]
    async fn tool_called_respects_max() {
        let events = vec![
            tool_event("tool__echo", "{}"),
            tool_event("tool__echo", "{}"),
        ];
        let steps_outputs = workflow::StepOutputs::new();
        let trajectory = TrajectoryContext {
            events: &events,
            steps_outputs: &steps_outputs,
            usage_total: None,
            cost_total: None,
        };
        let assertions = vec![Assertion::ToolCalled {
            name: "tool__echo".to_owned(),
            min: None,
            max: Some(1),
            args_jq: None,
        }];
        let failures = evaluate(
            &assertions,
            None,
            Some(&trajectory),
            "output",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("at most"));
    }

    #[tokio::test]
    async fn tool_called_checks_args_jq_against_at_least_one_matching_call() {
        let events = vec![
            tool_event("tool__echo", r#"{"text":"bye"}"#),
            tool_event("tool__echo", r#"{"text":"hi"}"#),
        ];
        let steps_outputs = workflow::StepOutputs::new();
        let trajectory = TrajectoryContext {
            events: &events,
            steps_outputs: &steps_outputs,
            usage_total: None,
            cost_total: None,
        };
        let assertions = vec![Assertion::ToolCalled {
            name: "tool__echo".to_owned(),
            min: None,
            max: None,
            args_jq: Some(".text == \"hi\"".to_owned()),
        }];
        let failures = evaluate(
            &assertions,
            None,
            Some(&trajectory),
            "output",
            crate::cancellation::none(),
        )
        .await;
        assert!(failures.is_empty(), "failures: {failures:?}");
    }

    #[tokio::test]
    async fn tool_called_fails_without_a_trajectory_context() {
        let assertions = vec![Assertion::ToolCalled {
            name: "tool__echo".to_owned(),
            min: None,
            max: None,
            args_jq: None,
        }];
        let failures = evaluate(
            &assertions,
            None,
            None,
            "output",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("not supported"));
    }

    #[tokio::test]
    async fn usage_fails_when_total_tokens_exceed_the_maximum() {
        let events: Vec<trace::TraceEvent> = Vec::new();
        let steps_outputs = workflow::StepOutputs::new();
        let trajectory = TrajectoryContext {
            events: &events,
            steps_outputs: &steps_outputs,
            usage_total: Some(crate::response::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            cost_total: None,
        };
        let assertions = vec![Assertion::Usage {
            max_prompt_tokens: None,
            max_completion_tokens: None,
            max_total_tokens: Some(10),
            max_cost_usd: None,
        }];
        let failures = evaluate(
            &assertions,
            None,
            Some(&trajectory),
            "output",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("total_tokens"));
    }

    #[tokio::test]
    async fn usage_passes_within_bounds() {
        let events: Vec<trace::TraceEvent> = Vec::new();
        let steps_outputs = workflow::StepOutputs::new();
        let trajectory = TrajectoryContext {
            events: &events,
            steps_outputs: &steps_outputs,
            usage_total: Some(crate::response::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            cost_total: None,
        };
        let assertions = vec![Assertion::Usage {
            max_prompt_tokens: None,
            max_completion_tokens: None,
            max_total_tokens: Some(100),
            max_cost_usd: None,
        }];
        let failures = evaluate(
            &assertions,
            None,
            Some(&trajectory),
            "output",
            crate::cancellation::none(),
        )
        .await;
        assert!(failures.is_empty(), "failures: {failures:?}");
    }

    #[tokio::test]
    async fn step_output_checks_nested_assertions_against_the_named_steps_output() {
        let events: Vec<trace::TraceEvent> = Vec::new();
        let mut steps_outputs = workflow::StepOutputs::new();
        steps_outputs.insert(
            "greet".to_owned(),
            serde_json::Value::String("hello world".to_owned()),
        );
        let trajectory = TrajectoryContext {
            events: &events,
            steps_outputs: &steps_outputs,
            usage_total: None,
            cost_total: None,
        };
        let assertions = vec![Assertion::StepOutput {
            id: "greet".to_owned(),
            assert: vec![Assertion::Contains {
                value: "hello".to_owned(),
            }],
        }];
        let failures = evaluate(
            &assertions,
            None,
            Some(&trajectory),
            "final output",
            crate::cancellation::none(),
        )
        .await;
        assert!(failures.is_empty(), "failures: {failures:?}");
    }

    #[tokio::test]
    async fn step_output_fails_clearly_when_the_step_id_is_unknown() {
        let events: Vec<trace::TraceEvent> = Vec::new();
        let steps_outputs = workflow::StepOutputs::new();
        let trajectory = TrajectoryContext {
            events: &events,
            steps_outputs: &steps_outputs,
            usage_total: None,
            cost_total: None,
        };
        let assertions = vec![Assertion::StepOutput {
            id: "missing".to_owned(),
            assert: vec![Assertion::Contains {
                value: "hello".to_owned(),
            }],
        }];
        let failures = evaluate(
            &assertions,
            None,
            Some(&trajectory),
            "final output",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("produced no output"));
    }

    #[tokio::test]
    async fn step_output_fails_without_a_trajectory_context() {
        let assertions = vec![Assertion::StepOutput {
            id: "greet".to_owned(),
            assert: vec![],
        }];
        let failures = evaluate(
            &assertions,
            None,
            None,
            "final output",
            crate::cancellation::none(),
        )
        .await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("not supported"));
    }
}
