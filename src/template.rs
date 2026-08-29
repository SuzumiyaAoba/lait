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
/// (and, when `input` is an object, `{{ input.field }}` for nested access), and
/// `steps`, a map of step `id` to that step's recorded output (see
/// `workflow::StepOutputs`), exposed as `{{ steps.<id> }}` /
/// `{{ steps.<id>.field }}`. Referencing an undefined variable is an error
/// rather than an empty string. `{{ json input }}` (or `{{ json steps.<id> }}`)
/// renders a value as compact JSON text; handlebars' default bare rendering of
/// an object or array is the literal placeholder `[object]`/`[array]`, which is
/// rarely what's wanted, so a bare `{{ input }}` against an object/array input
/// is rejected up front rather than silently sending that placeholder text to
/// the model. The same guard does not apply to `steps`, since referencing a
/// specific step's output either names a field (`{{ steps.foo.bar }}`) or is
/// expected to be used with `{{ json steps.foo }}`.
pub(crate) fn render(
    template: &str,
    input: &serde_json::Value,
    steps: &serde_json::Map<String, serde_json::Value>,
) -> Result<String> {
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
        .render_template(template, &json!({ "input": input, "steps": steps }))
        .with_context(|| format!("failed to render template: {template:?}"))
}

/// Checks `template`'s handlebars syntax without rendering it (used by the
/// workflow/agent linter, which has no `input`/`steps` value to render
/// against yet). This only catches malformed `{{ ... }}`/block syntax; a
/// reference to an undefined variable, or a bare `{{ input }}` against an
/// object/array input, is only ever caught by `render`, at actual render time
/// against real data — a scalar `{{ input }}` (e.g. a first step's `prompt:`
/// run against a plain-text CLI argument) is perfectly valid, so flagging
/// every bare `{{ input }}` statically would be a false positive on one of
/// the most common templates in this codebase's own tests
/// (`renders_a_bare_input_placeholder_from_a_string`, below).
pub(crate) fn check_syntax(template: &str) -> Result<()> {
    handlebars::Template::compile(template)
        .map(|_| ())
        .with_context(|| format!("failed to parse template: {template:?}"))
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
    use super::{check_syntax, parse_input, references_bare_input, render};
    use serde_json::json;

    fn no_steps() -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    #[test]
    fn renders_a_bare_input_placeholder_from_a_string() {
        assert_eq!(
            render("summarize: {{ input }}", &parse_input("hello"), &no_steps()).unwrap(),
            "summarize: hello"
        );
    }

    #[test]
    fn renders_a_field_from_an_object_input() {
        let input = parse_input(r#"{"city":"Tokyo","population":37400000}"#);
        assert_eq!(
            render("city: {{ input.city }}", &input, &no_steps()).unwrap(),
            "city: Tokyo"
        );
    }

    #[test]
    fn renders_no_placeholder_text_unchanged() {
        assert_eq!(
            render("no placeholder here", &parse_input("x"), &no_steps()).unwrap(),
            "no placeholder here"
        );
    }

    #[test]
    fn rejects_an_undefined_variable() {
        assert!(render("{{ input.nope }}", &parse_input(r#"{"a":1}"#), &no_steps()).is_err());
        assert!(render("{{ nope }}", &parse_input("x"), &no_steps()).is_err());
    }

    #[test]
    fn rejects_an_unterminated_placeholder() {
        assert!(render("{{ input", &parse_input("x"), &no_steps()).is_err());
    }

    #[test]
    fn renders_a_whole_object_input_as_compact_json_via_the_json_helper() {
        let input = json!({"b": 2, "a": 1});
        assert_eq!(
            render("{{ json input }}", &input, &no_steps()).unwrap(),
            r#"{"b":2,"a":1}"#
        );
    }

    #[test]
    fn renders_a_nested_field_via_the_json_helper_when_it_is_itself_an_object() {
        let input = json!({"address": {"city": "Tokyo", "zip": "100-0001"}});
        assert_eq!(
            render("{{ json input.address }}", &input, &no_steps()).unwrap(),
            r#"{"city":"Tokyo","zip":"100-0001"}"#
        );
    }

    #[test]
    fn rejects_a_bare_input_placeholder_against_an_object_input() {
        let input = json!({"city": "Tokyo"});
        let error = render("{{ input }}", &input, &no_steps()).unwrap_err();
        assert!(error.to_string().contains("json input"));
    }

    #[test]
    fn rejects_a_bare_input_placeholder_against_an_array_input() {
        let input = json!(["Tokyo", "Osaka"]);
        assert!(render("{{ input }}", &input, &no_steps()).is_err());
    }

    #[test]
    fn allows_field_access_and_the_json_helper_against_an_object_input() {
        let input = json!({"city": "Tokyo"});
        assert!(render("{{ input.city }}", &input, &no_steps()).is_ok());
        assert!(render("{{ json input }}", &input, &no_steps()).is_ok());
    }

    #[test]
    fn renders_a_field_from_a_named_step_output() {
        let mut steps = no_steps();
        steps.insert("extract".to_owned(), json!({"city": "Tokyo"}));
        assert_eq!(
            render("city: {{ steps.extract.city }}", &parse_input("x"), &steps).unwrap(),
            "city: Tokyo"
        );
    }

    #[test]
    fn renders_a_whole_named_step_output_via_the_json_helper() {
        let mut steps = no_steps();
        steps.insert("extract".to_owned(), json!({"city": "Tokyo"}));
        assert_eq!(
            render("{{ json steps.extract }}", &parse_input("x"), &steps).unwrap(),
            r#"{"city":"Tokyo"}"#
        );
    }

    #[test]
    fn rejects_a_reference_to_an_unrecorded_step_id() {
        assert!(render("{{ steps.missing }}", &parse_input("x"), &no_steps()).is_err());
    }

    #[test]
    fn parse_input_falls_back_to_a_plain_string_for_non_json_text() {
        assert_eq!(parse_input("Alice"), json!("Alice"));
        assert_eq!(parse_input("42"), json!(42));
        assert_eq!(parse_input(r#"{"a":1}"#), json!({"a": 1}));
    }

    #[test]
    fn check_syntax_accepts_a_valid_template() {
        assert!(check_syntax("summarize: {{ input.city }}").is_ok());
    }

    #[test]
    fn check_syntax_accepts_a_template_with_no_placeholders() {
        assert!(check_syntax("plain text").is_ok());
    }

    #[test]
    fn check_syntax_rejects_an_unterminated_placeholder() {
        assert!(check_syntax("{{ input").is_err());
    }

    #[test]
    fn check_syntax_does_not_require_input_or_steps_values() {
        // Unlike `render`, `check_syntax` never resolves variables against
        // real data, so a template referencing an undefined variable still
        // passes a syntax-only check.
        assert!(check_syntax("{{ nope }}").is_ok());
    }

    #[test]
    fn references_bare_input_detects_a_standalone_input_placeholder() {
        assert!(references_bare_input("{{ input }}"));
        assert!(references_bare_input("summarize: {{ input }}"));
    }

    #[test]
    fn references_bare_input_ignores_field_access_and_helper_calls() {
        assert!(!references_bare_input("{{ input.city }}"));
        assert!(!references_bare_input("{{ json input }}"));
        assert!(!references_bare_input("no placeholder here"));
    }
}
