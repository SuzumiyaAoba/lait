use anyhow::{Result, anyhow};
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
    let outputs: Result<Vec<String>> = filter
        .id
        .run((ctx, input))
        .map(unwrap_valr)
        .map(|result| {
            result
                .map(render_val)
                .map_err(|error| anyhow!("jq filter {filter_source:?} failed: {error}"))
        })
        .collect();

    Ok(outputs?.join("\n"))
}

fn render_val(value: Val) -> String {
    match &value {
        Val::TStr(bytes) => jaq_json::bstr(&**bytes).to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::apply;

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
}
