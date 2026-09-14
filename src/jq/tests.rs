use std::sync::atomic::AtomicBool;

use super::{Steps, apply_bool, apply_cancellable, apply_one_cancellable, check_syntax};

fn no_steps() -> Steps {
    Steps::new()
}

fn no_vars() -> Steps {
    Steps::new()
}

fn steps_with(id: &str, value: serde_json::Value) -> Steps {
    let mut steps = Steps::new();
    steps.insert(id.to_owned(), value);
    steps
}

fn vars_with(key: &str, value: serde_json::Value) -> Steps {
    let mut vars = Steps::new();
    vars.insert(key.to_owned(), value);
    vars
}

#[test]
fn extracts_a_string_field_raw() {
    assert_eq!(
        apply_cancellable(
            ".name",
            r#"{"name":"Alice"}"#,
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        "Alice"
    );
}

#[test]
fn extracts_a_number_field_as_json() {
    assert_eq!(
        apply_cancellable(
            ".age",
            r#"{"age":30}"#,
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        "30"
    );
}

#[test]
fn joins_multiple_outputs_with_newlines() {
    assert_eq!(
        apply_cancellable(
            ".[]",
            r#"["a","b","c"]"#,
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        "a\nb\nc"
    );
}

#[test]
fn renders_objects_and_arrays_as_compact_json() {
    assert_eq!(
        apply_cancellable(
            "{n: .name}",
            r#"{"name":"Alice"}"#,
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        r#"{"n":"Alice"}"#
    );
}

#[test]
fn rejects_invalid_json_input() {
    assert!(
        apply_cancellable(
            ".",
            "not json",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}

#[test]
fn rejects_invalid_filter_syntax() {
    assert!(
        apply_cancellable(".[", "{}", &no_steps(), &no_vars(), &AtomicBool::new(false)).is_err()
    );
}

#[test]
fn reports_a_runtime_error_from_the_filter() {
    assert!(
        apply_cancellable(
            ".foo.bar",
            "1",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}

#[test]
fn apply_bool_treats_false_and_null_as_falsy() {
    assert!(!apply_bool(".flag", r#"{"flag":false}"#, &no_steps(), &no_vars()).unwrap());
    assert!(!apply_bool(".missing", "{}", &no_steps(), &no_vars()).unwrap());
}

#[test]
fn apply_bool_treats_everything_else_as_truthy() {
    assert!(apply_bool(".flag", r#"{"flag":true}"#, &no_steps(), &no_vars()).unwrap());
    assert!(apply_bool(".n", r#"{"n":0}"#, &no_steps(), &no_vars()).unwrap());
    assert!(apply_bool(".s", r#"{"s":""}"#, &no_steps(), &no_vars()).unwrap());
}

#[test]
fn apply_bool_rejects_zero_outputs() {
    assert!(apply_bool(".[]", "[]", &no_steps(), &no_vars()).is_err());
}

#[test]
fn apply_bool_rejects_multiple_outputs() {
    assert!(apply_bool(".[]", "[true, false]", &no_steps(), &no_vars()).is_err());
}

#[test]
fn apply_one_renders_a_string_output_as_quoted_json() {
    assert_eq!(
        apply_one_cancellable(
            ".name",
            r#"{"name":"Alice"}"#,
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        r#""Alice""#
    );
}

#[test]
fn apply_one_renders_an_array_output_as_compact_json() {
    assert_eq!(
        apply_one_cancellable(
            ".items",
            r#"{"items":[1,2,3]}"#,
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        "[1,2,3]"
    );
}

#[test]
fn apply_one_rejects_zero_outputs() {
    assert!(
        apply_one_cancellable(
            ".[]",
            "[]",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}

#[test]
fn apply_one_rejects_multiple_outputs() {
    assert!(
        apply_one_cancellable(
            ".[]",
            "[1, 2]",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}

#[test]
fn rejects_an_unbounded_number_of_outputs() {
    let error = apply_cancellable(
        "range(0; 100001)",
        "null",
        &no_steps(),
        &no_vars(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(error.to_string().contains("configured limit"));
}

#[test]
fn apply_bool_rejects_a_stream_after_the_second_value() {
    let error = apply_bool("range(0; 1000000000)", "null", &no_steps(), &no_vars()).unwrap_err();
    assert!(
        error.to_string().contains("produced 2 outputs"),
        "unexpected jq error: {error:#}"
    );
}

#[test]
fn apply_one_rejects_a_stream_after_the_second_value() {
    let error = apply_one_cancellable(
        "range(0; 1000000000)",
        "null",
        &no_steps(),
        &no_vars(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("produced 2 outputs"),
        "unexpected jq error: {error:#}"
    );
}

#[test]
fn rejects_rendered_output_larger_than_the_evaluation_limit() {
    let filter = format!("\"x\" * {}", super::MAX_RENDERED_BYTES + 1);
    let error = apply_cancellable(
        &filter,
        "null",
        &no_steps(),
        &no_vars(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("rendered output exceeds"),
        "unexpected jq error: {error:#}"
    );
}

#[test]
fn apply_one_rejects_rendered_output_larger_than_the_evaluation_limit() {
    let filter = format!("\"x\" * {}", super::MAX_RENDERED_BYTES + 1);
    let error = apply_one_cancellable(
        &filter,
        "null",
        &no_steps(),
        &no_vars(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("rendered output exceeds"),
        "unexpected jq error: {error:#}"
    );
}

#[test]
fn rejects_an_output_with_excessive_nesting() {
    let filter =
        (0..=super::MAX_VALUE_DEPTH).fold("null".to_owned(), |value, _| format!("[{value}]"));
    let error = apply_cancellable(
        &filter,
        "null",
        &no_steps(),
        &no_vars(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("nesting limit"),
        "unexpected jq error: {error:#}"
    );
}

#[test]
fn rejects_input_larger_than_the_evaluation_limit() {
    let input = format!("\"{}\"", "x".repeat(super::MAX_INPUT_BYTES));
    let error = apply_cancellable(
        ".",
        &input,
        &no_steps(),
        &no_vars(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(error.to_string().contains("input exceeds"));
}

#[test]
fn apply_can_reference_a_named_step_output_via_dollar_steps() {
    let steps = steps_with("extract", serde_json::json!({"city": "Tokyo"}));
    assert_eq!(
        apply_cancellable(
            "$steps.extract.city",
            "null",
            &steps,
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        "Tokyo"
    );
}

#[test]
fn apply_bool_can_reference_a_named_step_output_via_dollar_steps() {
    let steps = steps_with("check", serde_json::json!({"ok": true}));
    assert!(apply_bool("$steps.check.ok", "null", &steps, &no_vars()).unwrap());
}

#[test]
fn dollar_steps_is_an_empty_object_when_no_step_output_is_recorded() {
    assert_eq!(
        apply_cancellable(
            "$steps",
            "null",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        "{}"
    );
}

#[test]
fn apply_can_reference_a_var_via_dollar_vars() {
    let vars = vars_with("lang", serde_json::json!("英語"));
    assert_eq!(
        apply_cancellable(
            "$vars.lang",
            "null",
            &no_steps(),
            &vars,
            &AtomicBool::new(false)
        )
        .unwrap(),
        "英語"
    );
}

#[test]
fn dollar_vars_is_an_empty_object_when_no_var_is_set() {
    assert_eq!(
        apply_cancellable(
            "$vars",
            "null",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        "{}"
    );
}

#[test]
fn check_syntax_accepts_a_valid_filter() {
    assert!(super::check_syntax(".foo.bar").is_ok());
}

#[test]
fn check_syntax_accepts_a_filter_that_references_dollar_steps() {
    assert!(super::check_syntax("$steps.extract.city").is_ok());
}

#[test]
fn check_syntax_accepts_a_filter_that_references_dollar_vars() {
    assert!(super::check_syntax("$vars.lang").is_ok());
}

#[test]
fn check_syntax_rejects_malformed_syntax() {
    assert!(super::check_syntax(".[").is_err());
}

#[test]
fn check_syntax_does_not_require_a_value_to_run_against() {
    // Unlike `apply`/`apply_bool`, `check_syntax` never parses/evaluates
    // input, so a filter guaranteed to fail at runtime (dividing by a
    // field that isn't a number) still passes a syntax-only check.
    assert!(super::check_syntax(".foo / 0").is_ok());
}

/// P8-4 (A-1'): a filter whose source never mentions `$steps` must produce
/// the exact same result whether `$steps` is empty or holds a large amount
/// of data — `run_filter_with` skips converting `$steps` into a jaq value at
/// all in this case, so this pins that the skip is invisible to the filter's
/// actual output, not just "doesn't crash".
#[test]
fn filter_without_steps_reference_evaluates_identically() {
    let empty_steps = no_steps();
    let mut populated_steps = Steps::new();
    for index in 0..500 {
        populated_steps.insert(
            format!("step-{index}"),
            serde_json::json!({"ok": true, "payload": "x".repeat(200)}),
        );
    }

    let with_empty = apply_cancellable(
        ".value",
        r#"{"value": 42}"#,
        &empty_steps,
        &no_vars(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let with_populated = apply_cancellable(
        ".value",
        r#"{"value": 42}"#,
        &populated_steps,
        &no_vars(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(with_empty, with_populated);
    assert_eq!(with_empty, "42");
}

/// The regression counterpart to the previous test: a filter that
/// genuinely reads `$steps` (and, combined here, `$vars`) must keep working
/// once the skip is in place — the two globals' `uses_steps`/`uses_vars`
/// flags must each be set correctly, independently of each other.
#[test]
fn filter_referencing_steps_still_sees_them() {
    let steps = steps_with("check", serde_json::json!({"ok": true}));
    let vars = vars_with("lang", serde_json::json!("ja"));
    assert_eq!(
        apply_cancellable(
            "[$steps.check.ok, $vars.lang]",
            "null",
            &steps,
            &vars,
            &AtomicBool::new(false)
        )
        .unwrap(),
        r#"[true,"ja"]"#
    );
}

/// A filter that merely contains the *text* `$steps` inside a jq string
/// literal (not an actual variable reference) is not, and cannot be,
/// distinguished from a real reference by a plain substring search — the
/// filter still compiles and evaluates the same value it always did,
/// because the substring match only decides whether the global gets
/// *constructed*, never whether the filter's own logic can use it. This
/// pins that a false-positive match is harmless, not that it doesn't occur.
#[test]
fn string_literal_mentioning_steps_is_treated_conservatively() {
    assert_eq!(
        apply_cancellable(
            r#""$steps""#,
            "null",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false)
        )
        .unwrap(),
        "$steps"
    );
}

/// `Vars::new`'s two slots are positional (`$steps` first, `$vars` second —
/// see `compiled_filter`'s `with_global_vars`), not name-keyed, so a filter
/// referencing only the *second* slot while the first is left as the
/// substituted `Val::Null` (unused because this filter never mentions
/// `$steps`) must still read its own slot correctly.
#[test]
fn unused_global_slot_does_not_break_evaluation() {
    let vars = vars_with("lang", serde_json::json!("ja"));
    assert_eq!(
        apply_cancellable(
            "$vars.lang",
            "null",
            &no_steps(),
            &vars,
            &AtomicBool::new(false)
        )
        .unwrap(),
        "ja"
    );
}

/// `validate_filter_source`'s nesting-depth scan short-circuits on a cache
/// hit in `FILTER_CACHE` (P9-3 §B) — but a filter that *fails* that scan is
/// never cached (only a filter that goes on to compile successfully is), so
/// calling the same over-nested filter source twice must reject it both
/// times, not let the second call slip through as a false cache hit. Uses a
/// filter source distinct from every other test in this module (a unique
/// text is a fresh `FILTER_CACHE` key) so this test cannot pass merely
/// because some other test already cached an entry under the same key.
#[test]
fn an_over_nested_filter_source_is_rejected_on_every_call_not_only_the_first() {
    let over_nested_filter = "[".repeat(super::MAX_VALUE_DEPTH + 1);
    assert!(
        check_syntax(&over_nested_filter).is_err(),
        "first call must reject the over-nested filter"
    );
    assert!(
        check_syntax(&over_nested_filter).is_err(),
        "second call (a `FILTER_CACHE` lookup for the same source text) must \
         still reject it — a rejected filter must never be cached as valid"
    );
}
