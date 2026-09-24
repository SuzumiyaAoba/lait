use super::{Globals, eval_bool, eval_one};
use serde_json::json;

fn globals() -> Globals {
    Globals::default()
}

#[test]
fn extracts_a_string_field_as_a_json_string() {
    assert_eq!(
        eval_one(".name", &json!({"name": "Alice"}), &globals()).unwrap(),
        json!("Alice")
    );
}

#[test]
fn extracts_a_number_field() {
    assert_eq!(
        eval_one(".age", &json!({"age": 30}), &globals()).unwrap(),
        json!(30)
    );
}

#[test]
fn builds_objects_and_arrays() {
    assert_eq!(
        eval_one(
            "{n: .name, xs: [1, 2]}",
            &json!({"name": "Alice"}),
            &globals()
        )
        .unwrap(),
        json!({"n": "Alice", "xs": [1, 2]})
    );
}

#[test]
fn a_plain_string_input_is_a_json_string() {
    assert_eq!(
        eval_one("ascii_upcase", &json!("abc"), &globals()).unwrap(),
        json!("ABC")
    );
}

#[test]
fn rejects_invalid_filter_syntax() {
    assert!(eval_one(".[", &json!({}), &globals()).is_err());
}

#[test]
fn reports_a_runtime_error_from_the_filter() {
    assert!(eval_one(".foo.bar", &json!(1), &globals()).is_err());
}

#[test]
fn rejects_zero_outputs() {
    assert!(eval_one(".[]", &json!([]), &globals()).is_err());
    assert!(eval_bool(".[]", &json!([]), &globals()).is_err());
}

#[test]
fn rejects_multiple_outputs_and_suggests_collecting_them() {
    let error = eval_one(".[]", &json!([1, 2]), &globals()).unwrap_err();
    assert!(
        error.to_string().contains("'[...]'"),
        "unexpected jq error: {error:#}"
    );
    assert!(eval_bool(".[]", &json!([true, false]), &globals()).is_err());
}

#[test]
fn eval_bool_treats_false_and_null_as_falsy() {
    assert!(!eval_bool(".flag", &json!({"flag": false}), &globals()).unwrap());
    assert!(!eval_bool(".missing", &json!({}), &globals()).unwrap());
}

#[test]
fn eval_bool_treats_everything_else_as_truthy() {
    assert!(eval_bool(".flag", &json!({"flag": true}), &globals()).unwrap());
    assert!(eval_bool(".n", &json!({"n": 0}), &globals()).unwrap());
    assert!(eval_bool(".s", &json!({"s": ""}), &globals()).unwrap());
}

#[test]
fn rejects_a_stream_after_the_second_value() {
    let error = eval_one("range(0; 1000000000)", &json!(null), &globals()).unwrap_err();
    assert!(
        error.to_string().contains("produced 2 outputs"),
        "unexpected jq error: {error:#}"
    );
    let error = eval_bool("range(0; 1000000000)", &json!(null), &globals()).unwrap_err();
    assert!(error.to_string().contains("produced 2 outputs"));
}

#[test]
fn rejects_rendered_output_larger_than_the_evaluation_limit() {
    let filter = format!("\"x\" * {}", super::MAX_RENDERED_BYTES + 1);
    let error = eval_one(&filter, &json!(null), &globals()).unwrap_err();
    assert!(
        format!("{error:#}").contains("rendered output exceeds"),
        "unexpected jq error: {error:#}"
    );
}

#[test]
fn rejects_an_output_with_excessive_nesting() {
    let filter =
        (0..=super::MAX_VALUE_DEPTH).fold("null".to_owned(), |value, _| format!("[{value}]"));
    let error = eval_one(&filter, &json!(null), &globals()).unwrap_err();
    assert!(
        error.to_string().contains("nesting limit"),
        "unexpected jq error: {error:#}"
    );
}

#[test]
fn rejects_input_larger_than_the_evaluation_limit() {
    let input = json!("x".repeat(super::MAX_INPUT_BYTES));
    let error = eval_one(".", &input, &globals()).unwrap_err();
    assert!(error.to_string().contains("input exceeds"));
}

#[test]
fn can_reference_a_named_step_output_via_dollar_steps() {
    let mut globals = globals();
    globals
        .steps
        .insert("extract".to_owned(), json!({"city": "Tokyo"}));
    assert_eq!(
        eval_one("$steps.extract.city", &json!(null), &globals).unwrap(),
        json!("Tokyo")
    );
    assert!(eval_bool("$steps.extract.city == \"Tokyo\"", &json!(null), &globals).unwrap());
}

#[test]
fn can_reference_an_input_via_dollar_inputs() {
    let mut globals = globals();
    globals.inputs.insert("lang".to_owned(), json!("英語"));
    assert_eq!(
        eval_one("$inputs.lang", &json!(null), &globals).unwrap(),
        json!("英語")
    );
}

#[test]
fn can_reference_the_loop_context_via_dollar_loop() {
    let mut globals = globals();
    globals.loop_context = json!({"index": 2, "item": "c"});
    assert_eq!(
        eval_one("\"\\($loop.index):\\($loop.item)\"", &json!(null), &globals).unwrap(),
        json!("2:c")
    );
}

#[test]
fn globals_default_to_empty_objects_and_a_null_loop() {
    assert_eq!(
        eval_one("[$steps, $inputs, $loop]", &json!(null), &globals()).unwrap(),
        json!([{}, {}, null])
    );
}

#[test]
fn check_syntax_accepts_a_valid_filter() {
    assert!(super::check_syntax(".foo.bar").is_ok());
}

#[test]
fn check_syntax_accepts_every_global() {
    assert!(super::check_syntax("[$steps.a, $inputs.b, $loop.index]").is_ok());
}

#[test]
fn check_syntax_rejects_an_unknown_global() {
    assert!(super::check_syntax("$vars.lang").is_err());
}

#[test]
fn check_syntax_rejects_malformed_syntax() {
    assert!(super::check_syntax(".[").is_err());
}

#[test]
fn check_syntax_does_not_require_a_value_to_run_against() {
    assert!(super::check_syntax(".foo / 0").is_ok());
}

/// A filter whose source never mentions `$steps` must produce the exact
/// same result whether `$steps` is empty or holds a large amount of data —
/// `run_filter_with` skips converting `$steps` into a jaq value at all in
/// this case, so this pins that the skip is invisible to the filter's
/// actual output, not just "doesn't crash".
#[test]
fn filter_without_steps_reference_evaluates_identically() {
    let mut populated = globals();
    for index in 0..500 {
        populated.steps.insert(
            format!("step-{index}"),
            json!({"ok": true, "payload": "x".repeat(200)}),
        );
    }
    let input = json!({"value": 42});
    let with_empty = eval_one(".value", &input, &globals()).unwrap();
    let with_populated = eval_one(".value", &input, &populated).unwrap();
    assert_eq!(with_empty, with_populated);
    assert_eq!(with_empty, json!(42));
}

/// The regression counterpart to the previous test: a filter that
/// genuinely reads several globals must keep working once the skip is in
/// place — each global's `uses_*` flag must be set correctly, independently
/// of the others.
#[test]
fn filter_referencing_several_globals_still_sees_them() {
    let mut globals = globals();
    globals
        .steps
        .insert("check".to_owned(), json!({"ok": true}));
    globals.inputs.insert("lang".to_owned(), json!("ja"));
    globals.loop_context = json!({"index": 1, "item": "b"});
    assert_eq!(
        eval_one(
            "[$steps.check.ok, $inputs.lang, $loop.item]",
            &json!(null),
            &globals
        )
        .unwrap(),
        json!([true, "ja", "b"])
    );
}

/// A filter that merely contains the *text* `$steps` inside a jq string
/// literal is not distinguished from a real reference by the substring
/// search — the match only decides whether the global gets *constructed*,
/// never whether the filter's own logic can use it. This pins that a
/// false-positive match is harmless.
#[test]
fn string_literal_mentioning_steps_is_treated_conservatively() {
    assert_eq!(
        eval_one(r#""$steps""#, &json!(null), &globals()).unwrap(),
        json!("$steps")
    );
}

/// `Vars::new`'s slots are positional, not name-keyed, so a filter
/// referencing only a later slot while the earlier ones are left as the
/// substituted `Val::Null` must still read its own slot correctly.
#[test]
fn unused_global_slot_does_not_break_evaluation() {
    let mut globals = globals();
    globals.loop_context = json!({"index": 0, "item": "only"});
    assert_eq!(
        eval_one("$loop.item", &json!(null), &globals).unwrap(),
        json!("only")
    );
}

/// `validate_filter_source`'s nesting-depth scan short-circuits on a cache
/// hit in `FILTER_CACHE` — but a filter that *fails* that scan is never
/// cached, so calling the same over-nested filter source twice must reject
/// it both times, not let the second call slip through as a false cache
/// hit.
#[test]
fn an_over_nested_filter_source_is_rejected_on_every_call_not_only_the_first() {
    let over_nested_filter = "[".repeat(super::MAX_VALUE_DEPTH + 1);
    assert!(
        super::check_syntax(&over_nested_filter).is_err(),
        "first call must reject the over-nested filter"
    );
    assert!(
        super::check_syntax(&over_nested_filter).is_err(),
        "second call (a `FILTER_CACHE` lookup for the same source text) must \
         still reject it — a rejected filter must never be cached as valid"
    );
}
