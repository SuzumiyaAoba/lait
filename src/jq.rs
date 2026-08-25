use anyhow::{Result, anyhow, bail};
use jaq_core::{
    Compiler, Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::{Val, read};

/// Runs a jq filter against a single JSON input, rendering each output value as
/// text and joining multiple outputs with newlines (as `jq` does on the command
/// line). A string output is rendered raw/unquoted, like `jq -r`; every other
/// value is rendered as compact JSON.
pub(crate) fn apply(filter_source: &str, input_json: &str) -> Result<String> {
    let outputs = run_filter(filter_source, input_json)?;
    Ok(outputs
        .into_iter()
        .map(render_val)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Runs a jq filter as a boolean condition (used by workflow `when:` guards).
/// The filter must produce exactly one output value; that value is falsy iff
/// it is JSON `false` or `null` (jq's own truthiness rules), truthy otherwise.
pub(crate) fn apply_bool(filter_source: &str, input_json: &str) -> Result<bool> {
    let outputs = run_filter(filter_source, input_json)?;
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
pub(crate) fn apply_one(filter_source: &str, input_json: &str) -> Result<String> {
    let outputs = run_filter(filter_source, input_json)?;
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

fn run_filter(filter_source: &str, input_json: &str) -> Result<Vec<Val>> {
    let input = read::parse_single(input_json.as_bytes())
        .map_err(|error| anyhow!("failed to parse jq input as JSON: {error}"))?;

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
        .compile(modules)
        .map_err(|errors| anyhow!("failed to compile jq filter {filter_source:?}: {errors:?}"))?;

    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
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
    use super::{apply, apply_bool, apply_one};

    #[test]
    fn extracts_a_string_field_raw() {
        assert_eq!(apply(".name", r#"{"name":"Alice"}"#).unwrap(), "Alice");
    }

    #[test]
    fn extracts_a_number_field_as_json() {
        assert_eq!(apply(".age", r#"{"age":30}"#).unwrap(), "30");
    }

    #[test]
    fn joins_multiple_outputs_with_newlines() {
        assert_eq!(apply(".[]", r#"["a","b","c"]"#).unwrap(), "a\nb\nc");
    }

    #[test]
    fn renders_objects_and_arrays_as_compact_json() {
        assert_eq!(
            apply("{n: .name}", r#"{"name":"Alice"}"#).unwrap(),
            r#"{"n":"Alice"}"#
        );
    }

    #[test]
    fn rejects_invalid_json_input() {
        assert!(apply(".", "not json").is_err());
    }

    #[test]
    fn rejects_invalid_filter_syntax() {
        assert!(apply(".[", "{}").is_err());
    }

    #[test]
    fn reports_a_runtime_error_from_the_filter() {
        assert!(apply(".foo.bar", "1").is_err());
    }

    #[test]
    fn apply_bool_treats_false_and_null_as_falsy() {
        assert!(!apply_bool(".flag", r#"{"flag":false}"#).unwrap());
        assert!(!apply_bool(".missing", "{}").unwrap());
    }

    #[test]
    fn apply_bool_treats_everything_else_as_truthy() {
        assert!(apply_bool(".flag", r#"{"flag":true}"#).unwrap());
        assert!(apply_bool(".n", r#"{"n":0}"#).unwrap());
        assert!(apply_bool(".s", r#"{"s":""}"#).unwrap());
    }

    #[test]
    fn apply_bool_rejects_zero_outputs() {
        assert!(apply_bool(".[]", "[]").is_err());
    }

    #[test]
    fn apply_bool_rejects_multiple_outputs() {
        assert!(apply_bool(".[]", "[true, false]").is_err());
    }

    #[test]
    fn apply_one_renders_a_string_output_as_quoted_json() {
        assert_eq!(
            apply_one(".name", r#"{"name":"Alice"}"#).unwrap(),
            r#""Alice""#
        );
    }

    #[test]
    fn apply_one_renders_an_array_output_as_compact_json() {
        assert_eq!(
            apply_one(".items", r#"{"items":[1,2,3]}"#).unwrap(),
            "[1,2,3]"
        );
    }

    #[test]
    fn apply_one_rejects_zero_outputs() {
        assert!(apply_one(".[]", "[]").is_err());
    }

    #[test]
    fn apply_one_rejects_multiple_outputs() {
        assert!(apply_one(".[]", "[1, 2]").is_err());
    }
}
