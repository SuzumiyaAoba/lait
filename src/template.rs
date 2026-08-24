use anyhow::{Context, Result, bail};
use handlebars::{Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason};
use serde_json::json;

/// Parses a raw string as JSON when possible; falls back to a JSON string
/// holding the raw value unchanged (so a plain-text `{{ input }}` render is
/// unaffected regardless of whether the input happens to look like JSON).
pub(crate) fn parse_input(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
}

/// Renders `template` against `input`, exposed to the template as `{{ input }}`
/// (and, when `input` is an object, `{{ input.field }}` for nested access).
/// Referencing an undefined variable is an error rather than an empty string.
/// `{{ json input }}` (or `{{ json input.field }}`) renders a value as compact
/// JSON text; handlebars' default bare rendering of an object or array is the
/// literal placeholder `[object]`/`[array]`, which is rarely what's wanted, so
/// a bare `{{ input }}` against an object/array input is rejected up front
/// rather than silently sending that placeholder text to the model.
pub(crate) fn render(template: &str, input: &serde_json::Value) -> Result<String> {
    if matches!(
        input,
        serde_json::Value::Object(_) | serde_json::Value::Array(_)
    ) && references_bare_input(template)
    {
        bail!(
            "template references bare '{{{{ input }}}}' but the input is a JSON object/array; \
             use '{{{{ json input }}}}' to render it as JSON text, or access a field with \
             '{{{{ input.field }}}}'"
        );
    }

    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars.register_helper("json", Box::new(json_helper));
    handlebars
        .render_template(template, &json!({ "input": input }))
        .with_context(|| format!("failed to render template: {template:?}"))
}

/// Whether `template` contains a bare `{{ input }}` expression (as opposed to
/// `{{ input.field }}` or a helper call like `{{ json input }}`).
fn references_bare_input(template: &str) -> bool {
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return false;
        };
        if after[..end].trim() == "input" {
            return true;
        }
        rest = &after[end + 2..];
    }
    false
}

fn json_helper(
    helper: &Helper,
    _: &Handlebars,
    _: &handlebars::Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    let value = helper
        .param(0)
        .ok_or_else(|| RenderErrorReason::ParamNotFoundForIndex("json", 0))?
        .value();
    out.write(&serde_json::to_string(value).map_err(RenderErrorReason::SerdeError)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_input, render};
    use serde_json::json;

    #[test]
    fn renders_a_bare_input_placeholder_from_a_string() {
        assert_eq!(
            render("summarize: {{ input }}", &parse_input("hello")).unwrap(),
            "summarize: hello"
        );
    }

    #[test]
    fn renders_a_field_from_an_object_input() {
        let input = parse_input(r#"{"city":"Tokyo","population":37400000}"#);
        assert_eq!(
            render("city: {{ input.city }}", &input).unwrap(),
            "city: Tokyo"
        );
    }

    #[test]
    fn renders_no_placeholder_text_unchanged() {
        assert_eq!(
            render("no placeholder here", &parse_input("x")).unwrap(),
            "no placeholder here"
        );
    }

    #[test]
    fn rejects_an_undefined_variable() {
        assert!(render("{{ input.nope }}", &parse_input(r#"{"a":1}"#)).is_err());
        assert!(render("{{ nope }}", &parse_input("x")).is_err());
    }

    #[test]
    fn rejects_an_unterminated_placeholder() {
        assert!(render("{{ input", &parse_input("x")).is_err());
    }

    #[test]
    fn renders_a_whole_object_input_as_compact_json_via_the_json_helper() {
        let input = json!({"b": 2, "a": 1});
        assert_eq!(
            render("{{ json input }}", &input).unwrap(),
            r#"{"b":2,"a":1}"#
        );
    }

    #[test]
    fn renders_a_nested_field_via_the_json_helper_when_it_is_itself_an_object() {
        let input = json!({"address": {"city": "Tokyo", "zip": "100-0001"}});
        assert_eq!(
            render("{{ json input.address }}", &input).unwrap(),
            r#"{"city":"Tokyo","zip":"100-0001"}"#
        );
    }

    #[test]
    fn rejects_a_bare_input_placeholder_against_an_object_input() {
        let input = json!({"city": "Tokyo"});
        let error = render("{{ input }}", &input).unwrap_err();
        assert!(error.to_string().contains("json input"));
    }

    #[test]
    fn rejects_a_bare_input_placeholder_against_an_array_input() {
        let input = json!(["Tokyo", "Osaka"]);
        assert!(render("{{ input }}", &input).is_err());
    }

    #[test]
    fn allows_field_access_and_the_json_helper_against_an_object_input() {
        let input = json!({"city": "Tokyo"});
        assert!(render("{{ input.city }}", &input).is_ok());
        assert!(render("{{ json input }}", &input).is_ok());
    }

    #[test]
    fn parse_input_falls_back_to_a_plain_string_for_non_json_text() {
        assert_eq!(parse_input("Alice"), json!("Alice"));
        assert_eq!(parse_input("42"), json!(42));
        assert_eq!(parse_input(r#"{"a":1}"#), json!({"a": 1}));
    }
}
