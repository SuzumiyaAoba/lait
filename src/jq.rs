use anyhow::{Result, anyhow, bail};
use jaq_core::{
    Compiler, Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::{Val, read};

/// Named step outputs recorded by `id` (see `workflow::StepOutputs`), exposed
/// to jq filters as the `$steps` global variable (e.g. `$steps.extract.city`).
pub(crate) type Steps = serde_json::Map<String, serde_json::Value>;

/// Runs a jq filter against a single JSON input, rendering each output value as
/// text and joining multiple outputs with newlines (as `jq` does on the command
/// line). A string output is rendered raw/unquoted, like `jq -r`; every other
/// value is rendered as compact JSON.
pub(crate) fn apply(filter_source: &str, input_json: &str, steps: &Steps) -> Result<String> {
    let outputs = run_filter(filter_source, input_json, steps)?;
    Ok(outputs
        .into_iter()
        .map(render_val)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Runs a jq filter as a boolean condition (used by workflow `when:` guards).
/// The filter must produce exactly one output value; that value is falsy iff
/// it is JSON `false` or `null` (jq's own truthiness rules), truthy otherwise.
pub(crate) fn apply_bool(filter_source: &str, input_json: &str, steps: &Steps) -> Result<bool> {
    let outputs = run_filter(filter_source, input_json, steps)?;
    match outputs.as_slice() {
        [] => {
            bail!("jq condition {filter_source:?} produced no output; expected exactly one value")
        }
        [value] => Ok(!matches!(value, Val::Null | Val::Bool(false))),
        _ => bail!(
            "jq condition {filter_source:?} produced {} outputs; expected exactly one value",
            outputs.len()
        ),
    }
}

/// Runs a jq filter that must produce exactly one JSON value (used by
/// workflow `for_each.items:` filters). Unlike `apply`, the result is
/// rendered as proper JSON text even for a string output (no `jq -r`-style
/// unquoting), and multiple outputs are rejected instead of newline-joined.
pub(crate) fn apply_one(filter_source: &str, input_json: &str, steps: &Steps) -> Result<String> {
    let outputs = run_filter(filter_source, input_json, steps)?;
    match outputs.as_slice() {
        [] => {
            bail!("jq filter {filter_source:?} produced no output; expected exactly one value")
        }
        [value] => Ok(value.to_string()),
        _ => bail!(
            "jq filter {filter_source:?} produced {} outputs; expected exactly one value",
            outputs.len()
        ),
    }
}

/// Parses and compiles `filter_source` without running it against any input,
/// to check its syntax statically (used by the workflow/agent linter, which
/// has no `$steps`/input value at hand yet). Mirrors the parse/compile half
/// of `run_filter` (kept as its own copy rather than factored out: the
/// compiled filter's type borrows from the local `arena`, so sharing it back
/// out to a caller isn't worth the lifetime plumbing for a check that never
/// needs the result). A filter that only references `$steps` still compiles
/// here, since `with_global_vars` is declared the same way `run_filter` does.
pub(crate) fn check_syntax(filter_source: &str) -> Result<()> {
    let program = File {
        code: filter_source,
        path: (),
    };
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    // `run_filter` leaves this turbofish off: its later `Ctx::<data::JustLut<Val>>`
    // pins `D` retroactively. `check_syntax` never builds a `Ctx` (it only
    // compiles, never runs, the filter), so nothing else fixes `D` — spell it
    // out instead.
    let funs = jaq_core::funs::<data::JustLut<Val>>()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(&arena, program)
        .map_err(|errors| anyhow!("failed to parse jq filter {filter_source:?}: {errors:?}"))?;
    Compiler::default()
        .with_funs(funs)
        .with_global_vars(["$steps"])
        .compile(modules)
        .map_err(|errors| anyhow!("failed to compile jq filter {filter_source:?}: {errors:?}"))?;
    Ok(())
}

fn run_filter(filter_source: &str, input_json: &str, steps: &Steps) -> Result<Vec<Val>> {
    let input = read::parse_single(input_json.as_bytes())
        .map_err(|error| anyhow!("failed to parse jq input as JSON: {error}"))?;
    let steps_json = serde_json::to_string(steps)
        .map_err(|error| anyhow!("failed to serialize named step outputs for '$steps': {error}"))?;
    let steps_val = read::parse_single(steps_json.as_bytes())
        .map_err(|error| anyhow!("failed to parse named step outputs as JSON: {error}"))?;

    let program = File {
        code: filter_source,
        path: (),
    };
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(&arena, program)
        .map_err(|errors| anyhow!("failed to parse jq filter {filter_source:?}: {errors:?}"))?;
    let filter = Compiler::default()
        .with_funs(funs)
        .with_global_vars(["$steps"])
        .compile(modules)
        .map_err(|errors| anyhow!("failed to compile jq filter {filter_source:?}: {errors:?}"))?;

    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([steps_val]));
    filter
        .id
        .run((ctx, input))
        .map(unwrap_valr)
        .map(|result| {
            result.map_err(|error| anyhow!("jq filter {filter_source:?} failed: {error}"))
        })
        .collect()
}

fn render_val(value: Val) -> String {
    match &value {
        Val::TStr(bytes) => jaq_json::bstr(&**bytes).to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Steps, apply, apply_bool, apply_one};

    fn no_steps() -> Steps {
        Steps::new()
    }

    fn steps_with(id: &str, value: serde_json::Value) -> Steps {
        let mut steps = Steps::new();
        steps.insert(id.to_owned(), value);
        steps
    }

    #[test]
    fn extracts_a_string_field_raw() {
        assert_eq!(
            apply(".name", r#"{"name":"Alice"}"#, &no_steps()).unwrap(),
            "Alice"
        );
    }

    #[test]
    fn extracts_a_number_field_as_json() {
        assert_eq!(apply(".age", r#"{"age":30}"#, &no_steps()).unwrap(), "30");
    }

    #[test]
    fn joins_multiple_outputs_with_newlines() {
        assert_eq!(
            apply(".[]", r#"["a","b","c"]"#, &no_steps()).unwrap(),
            "a\nb\nc"
        );
    }

    #[test]
    fn renders_objects_and_arrays_as_compact_json() {
        assert_eq!(
            apply("{n: .name}", r#"{"name":"Alice"}"#, &no_steps()).unwrap(),
            r#"{"n":"Alice"}"#
        );
    }

    #[test]
    fn rejects_invalid_json_input() {
        assert!(apply(".", "not json", &no_steps()).is_err());
    }

    #[test]
    fn rejects_invalid_filter_syntax() {
        assert!(apply(".[", "{}", &no_steps()).is_err());
    }

    #[test]
    fn reports_a_runtime_error_from_the_filter() {
        assert!(apply(".foo.bar", "1", &no_steps()).is_err());
    }

    #[test]
    fn apply_bool_treats_false_and_null_as_falsy() {
        assert!(!apply_bool(".flag", r#"{"flag":false}"#, &no_steps()).unwrap());
        assert!(!apply_bool(".missing", "{}", &no_steps()).unwrap());
    }

    #[test]
    fn apply_bool_treats_everything_else_as_truthy() {
        assert!(apply_bool(".flag", r#"{"flag":true}"#, &no_steps()).unwrap());
        assert!(apply_bool(".n", r#"{"n":0}"#, &no_steps()).unwrap());
        assert!(apply_bool(".s", r#"{"s":""}"#, &no_steps()).unwrap());
    }

    #[test]
    fn apply_bool_rejects_zero_outputs() {
        assert!(apply_bool(".[]", "[]", &no_steps()).is_err());
    }

    #[test]
    fn apply_bool_rejects_multiple_outputs() {
        assert!(apply_bool(".[]", "[true, false]", &no_steps()).is_err());
    }

    #[test]
    fn apply_one_renders_a_string_output_as_quoted_json() {
        assert_eq!(
            apply_one(".name", r#"{"name":"Alice"}"#, &no_steps()).unwrap(),
            r#""Alice""#
        );
    }

    #[test]
    fn apply_one_renders_an_array_output_as_compact_json() {
        assert_eq!(
            apply_one(".items", r#"{"items":[1,2,3]}"#, &no_steps()).unwrap(),
            "[1,2,3]"
        );
    }

    #[test]
    fn apply_one_rejects_zero_outputs() {
        assert!(apply_one(".[]", "[]", &no_steps()).is_err());
    }

    #[test]
    fn apply_one_rejects_multiple_outputs() {
        assert!(apply_one(".[]", "[1, 2]", &no_steps()).is_err());
    }

    #[test]
    fn apply_can_reference_a_named_step_output_via_dollar_steps() {
        let steps = steps_with("extract", serde_json::json!({"city": "Tokyo"}));
        assert_eq!(
            apply("$steps.extract.city", "null", &steps).unwrap(),
            "Tokyo"
        );
    }

    #[test]
    fn apply_bool_can_reference_a_named_step_output_via_dollar_steps() {
        let steps = steps_with("check", serde_json::json!({"ok": true}));
        assert!(apply_bool("$steps.check.ok", "null", &steps).unwrap());
    }

    #[test]
    fn dollar_steps_is_an_empty_object_when_no_step_output_is_recorded() {
        assert_eq!(apply("$steps", "null", &no_steps()).unwrap(), "{}");
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
}
