//! Handlebars rendering for a workflow node's `prompt:`/`system_prompt:`/
//! argv templates: `{{ input }}`/`{{ steps.<id> }}`/`{{ vars.<key> }}`
//! placeholders and the `{{ json ... }}` helper for embedding a value as
//! compact JSON text. [`RenderScope`] is the entry point a caller rendering
//! more than one template against the same `input`/`steps`/`vars` should
//! use directly (see its doc comment); [`render`] is a one-shot convenience
//! wrapper around it. Both go through [`compiled_template`]'s process-wide
//! cache rather than recompiling a template string on every render.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::{Context, Result, bail};
use handlebars::{Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason};
use handlebars::{Renderable, Template};

/// Parses a raw string as JSON when possible; falls back to a JSON string
/// holding the raw value unchanged (so a plain-text `{{ input }}` render is
/// unaffected regardless of whether the input happens to look like JSON).
pub(crate) fn parse_input(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
}

/// Renders `template` against `input`/`steps`/`vars` in one shot — a
/// convenience wrapper around [`RenderScope`] for a caller that renders
/// only one template against one set of data. A caller that renders
/// several templates against the *same* `input`/`steps`/`vars` (a prompt
/// node's `prompt:` and `system_prompt:`, or a command node's argv list)
/// should build one `RenderScope` and call `.render()` on it repeatedly
/// instead, so the data is only cloned into handlebars' `Context` once. See
/// `RenderScope` for what each placeholder resolves to and the `{{ json }}`
/// helper.
pub(crate) fn render(
    template: &str,
    input: &serde_json::Value,
    steps: &serde_json::Map<String, serde_json::Value>,
    vars: &serde_json::Map<String, serde_json::Value>,
) -> Result<String> {
    RenderScope::new(input, steps, vars).render(template)
}

/// One `{ input, steps, vars }` rendering context, reusable across every
/// template rendered against the same data.
///
/// `input` is exposed to a template as `{{ input }}` (and, when `input` is
/// an object, `{{ input.field }}` for nested access), `steps` as a map of
/// step `id` to that step's recorded output (see `workflow::StepOutputs`),
/// exposed as `{{ steps.<id> }}` / `{{ steps.<id>.field }}`, and `vars` as a
/// named prompt's `vars:` defaults merged with its call's `--var
/// key=value` overrides (see `prompt::build_vars`; empty for every caller
/// but a named prompt), exposed as `{{ vars.<key> }}`. Referencing an
/// undefined variable is an error rather than an empty string. `{{ json
/// input }}` (or `{{ json steps.<id> }}`) renders a value as compact JSON
/// text; handlebars' default bare rendering of an object or array is the
/// literal placeholder `[object]`/`[array]`, which is rarely what's
/// wanted, so a bare `{{ input }}` against an object/array input is
/// rejected up front rather than silently sending that placeholder text to
/// the model. The same guard does not apply to `steps`/`vars`, since
/// referencing one of their fields either names a field (`{{
/// steps.foo.bar }}`/`{{ vars.lang }}`) or is expected to be used with
/// `{{ json steps.foo }}`.
///
/// Building the underlying handlebars `Context` clones `input`/`steps`/
/// `vars` into an owned `serde_json::Value` tree — `handlebars::Context`
/// always owns its data (see `Context::from<Json>`/`Context::wraps`, which
/// both take the value by move) — so this clone happens once per
/// `RenderScope::new` call, not once per template rendered against it.
pub(crate) struct RenderScope {
    context: handlebars::Context,
    input_is_object_or_array: bool,
}

impl RenderScope {
    pub(crate) fn new(
        input: &serde_json::Value,
        steps: &serde_json::Map<String, serde_json::Value>,
        vars: &serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let input_is_object_or_array = matches!(
            input,
            serde_json::Value::Object(_) | serde_json::Value::Array(_)
        );
        let mut data = serde_json::Map::with_capacity(3);
        data.insert("input".to_owned(), input.clone());
        data.insert("steps".to_owned(), serde_json::Value::Object(steps.clone()));
        data.insert("vars".to_owned(), serde_json::Value::Object(vars.clone()));
        Self {
            context: handlebars::Context::from(serde_json::Value::Object(data)),
            input_is_object_or_array,
        }
    }

    /// Renders `template` against this scope's data. Equivalent to
    /// `HANDLEBARS.render_template(template, &data)`, but the template
    /// text is compiled once (cached in `TEMPLATE_CACHE`, keyed by source
    /// text) and reused across every call — including from a different
    /// `RenderScope` — instead of being re-parsed on every render.
    pub(crate) fn render(&self, template: &str) -> Result<String> {
        if self.input_is_object_or_array && references_bare_input(template) {
            bail!(
                "template references bare '{{{{ input }}}}' but the input is a JSON object/array; \
                 use '{{{{ json input }}}}' to render it as JSON text, or access a field with \
                 '{{{{ input.field }}}}'"
            );
        }

        let compiled = compiled_template(template)?;
        // Equivalent to `Handlebars::render_resolved_template_to_output`'s
        // non-dev-mode path for an unregistered (ad-hoc) template: `None`
        // as the root template name (an ad-hoc `Template::compile` result
        // has no name) and the registry's default (unset)
        // `recursive_lookup`, which `HANDLEBARS` never turns on.
        let mut render_context = RenderContext::new(None);
        compiled
            .renders(&HANDLEBARS, &self.context, &mut render_context)
            .with_context(|| format!("failed to render template: {template:?}"))
    }
}

/// The registry every render call shares: nothing about it depends on the
/// template or data being rendered (strict mode, no escaping, the `json`
/// helper), so it is built once instead of re-registered per call.
static HANDLEBARS: LazyLock<Handlebars<'static>> = LazyLock::new(|| {
    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars.register_helper("json", Box::new(json_helper));
    handlebars
});

/// Templates compiled by [`compiled_template`], shared across every render
/// in the process. `Handlebars::render_template`/`render_template_with_context`
/// compile their argument from scratch on every call (there is no built-in
/// cache for an ad-hoc, unregistered template string); caching the compiled
/// result here means a template text rendered more than once — most
/// commonly a `for_each`/`loop` body's `prompt:`/`system_prompt:` re-run
/// per iteration — is only parsed the first time. Deliberately left
/// unbounded, the same way `jq::FILTER_CACHE` is: for a single `lait
/// run`/`lait chat` process, a workflow's set of distinct template strings
/// is fixed at parse time. `lait lint <DIR>` is the one case where this
/// grows across every workflow file in a directory tree rather than one
/// workflow — still bounded by the distinct template strings on disk, and
/// the process exits once linting finishes.
static TEMPLATE_CACHE: LazyLock<Mutex<HashMap<String, Arc<Template>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn compiled_template(template: &str) -> Result<Arc<Template>> {
    if let Some(compiled) = TEMPLATE_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(template)
    {
        return Ok(Arc::clone(compiled));
    }

    let compiled = Template::compile(template)
        .with_context(|| format!("failed to parse template: {template:?}"))?;
    let compiled = Arc::new(compiled);
    TEMPLATE_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(template.to_owned(), Arc::clone(&compiled));
    Ok(compiled)
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
/// (`renders_a_bare_input_placeholder_from_a_string`, below). Goes through
/// the same `compiled_template` cache `RenderScope::render` uses, so a
/// template the linter already checked doesn't pay the parse cost twice.
pub(crate) fn check_syntax(template: &str) -> Result<()> {
    compiled_template(template).map(|_| ())
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
    use super::{RenderScope, check_syntax, parse_input, references_bare_input, render};
    use serde_json::json;

    fn no_steps() -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    fn no_vars() -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    #[test]
    fn renders_a_bare_input_placeholder_from_a_string() {
        assert_eq!(
            render(
                "summarize: {{ input }}",
                &parse_input("hello"),
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            "summarize: hello"
        );
    }

    #[test]
    fn renders_a_field_from_an_object_input() {
        let input = parse_input(r#"{"city":"Tokyo","population":37400000}"#);
        assert_eq!(
            render("city: {{ input.city }}", &input, &no_steps(), &no_vars()).unwrap(),
            "city: Tokyo"
        );
    }

    #[test]
    fn renders_no_placeholder_text_unchanged() {
        assert_eq!(
            render(
                "no placeholder here",
                &parse_input("x"),
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            "no placeholder here"
        );
    }

    #[test]
    fn rejects_an_undefined_variable() {
        assert!(
            render(
                "{{ input.nope }}",
                &parse_input(r#"{"a":1}"#),
                &no_steps(),
                &no_vars()
            )
            .is_err()
        );
        assert!(render("{{ nope }}", &parse_input("x"), &no_steps(), &no_vars()).is_err());
    }

    #[test]
    fn rejects_an_unterminated_placeholder() {
        assert!(render("{{ input", &parse_input("x"), &no_steps(), &no_vars()).is_err());
    }

    #[test]
    fn renders_a_whole_object_input_as_compact_json_via_the_json_helper() {
        let input = json!({"b": 2, "a": 1});
        assert_eq!(
            render("{{ json input }}", &input, &no_steps(), &no_vars()).unwrap(),
            r#"{"b":2,"a":1}"#
        );
    }

    #[test]
    fn renders_a_nested_field_via_the_json_helper_when_it_is_itself_an_object() {
        let input = json!({"address": {"city": "Tokyo", "zip": "100-0001"}});
        assert_eq!(
            render("{{ json input.address }}", &input, &no_steps(), &no_vars()).unwrap(),
            r#"{"city":"Tokyo","zip":"100-0001"}"#
        );
    }

    #[test]
    fn rejects_a_bare_input_placeholder_against_an_object_input() {
        let input = json!({"city": "Tokyo"});
        let error = render("{{ input }}", &input, &no_steps(), &no_vars()).unwrap_err();
        assert!(error.to_string().contains("json input"));
    }

    #[test]
    fn rejects_a_bare_input_placeholder_against_an_array_input() {
        let input = json!(["Tokyo", "Osaka"]);
        assert!(render("{{ input }}", &input, &no_steps(), &no_vars()).is_err());
    }

    #[test]
    fn allows_field_access_and_the_json_helper_against_an_object_input() {
        let input = json!({"city": "Tokyo"});
        assert!(render("{{ input.city }}", &input, &no_steps(), &no_vars()).is_ok());
        assert!(render("{{ json input }}", &input, &no_steps(), &no_vars()).is_ok());
    }

    #[test]
    fn renders_a_field_from_a_named_step_output() {
        let mut steps = no_steps();
        steps.insert("extract".to_owned(), json!({"city": "Tokyo"}));
        assert_eq!(
            render(
                "city: {{ steps.extract.city }}",
                &parse_input("x"),
                &steps,
                &no_vars()
            )
            .unwrap(),
            "city: Tokyo"
        );
    }

    #[test]
    fn renders_a_whole_named_step_output_via_the_json_helper() {
        let mut steps = no_steps();
        steps.insert("extract".to_owned(), json!({"city": "Tokyo"}));
        assert_eq!(
            render(
                "{{ json steps.extract }}",
                &parse_input("x"),
                &steps,
                &no_vars()
            )
            .unwrap(),
            r#"{"city":"Tokyo"}"#
        );
    }

    #[test]
    fn renders_a_var_placeholder() {
        let mut vars = no_vars();
        vars.insert("lang".to_owned(), json!("英語"));
        assert_eq!(
            render("{{ vars.lang }}", &parse_input("x"), &no_steps(), &vars).unwrap(),
            "英語"
        );
    }

    #[test]
    fn rejects_a_reference_to_an_unset_var() {
        assert!(
            render(
                "{{ vars.missing }}",
                &parse_input("x"),
                &no_steps(),
                &no_vars()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_a_reference_to_an_unrecorded_step_id() {
        assert!(
            render(
                "{{ steps.missing }}",
                &parse_input("x"),
                &no_steps(),
                &no_vars()
            )
            .is_err()
        );
    }

    #[test]
    fn rendering_the_same_template_text_twice_through_the_compiled_cache_agrees() {
        // Exercises `compiled_template`'s cache: the second call hits the
        // cache instead of recompiling, and must still produce the same
        // output as the first.
        let input = parse_input(r#"{"city":"Tokyo"}"#);
        let first = render("city: {{ input.city }}", &input, &no_steps(), &no_vars()).unwrap();
        let second = render("city: {{ input.city }}", &input, &no_steps(), &no_vars()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, "city: Tokyo");
    }

    #[test]
    fn a_render_scope_renders_multiple_templates_against_the_same_data() {
        // `RenderScope` builds its handlebars `Context` once and reuses it
        // across `.render()` calls — check that doesn't leak state between
        // the two renders or otherwise corrupt either result.
        let input = parse_input(r#"{"city":"Tokyo","population":37400000}"#);
        let scope = RenderScope::new(&input, &no_steps(), &no_vars());
        assert_eq!(scope.render("{{ input.city }}").unwrap(), "Tokyo");
        assert_eq!(scope.render("{{ input.population }}").unwrap(), "37400000");
        // Same template text as an earlier top-level test, rendered through
        // a different `RenderScope` — the shared `TEMPLATE_CACHE` entry
        // must not carry data from one scope into another.
        assert_eq!(scope.render("{{ input.city }}").unwrap(), "Tokyo");
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
