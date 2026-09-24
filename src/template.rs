//! Handlebars rendering for workflow/agent/prompt templates: `{{ input }}`,
//! `{{ steps.<id> }}`, `{{ inputs.<name> }}`, `{{ loop.* }}` (workflow
//! templates, see [`render_in`]) or `{{ vars.<key> }}` (named prompts, see
//! [`render`]/[`RenderScope`]) placeholders, and the `{{ json ... }}` helper
//! for embedding a value as JSON text. A bare placeholder renders its
//! value's text form (see [`to_text`]). Every render goes through
//! [`compiled_template`]'s process-wide cache rather than recompiling a
//! template string on every render.

use std::{
    borrow::Cow,
    sync::{Arc, LazyLock},
};

use anyhow::{Context, Result};
use handlebars::{
    Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason, Renderable,
    Template,
};
use serde::Serialize;

use crate::{jq, sync_cache::SyncCache};

/// Parses a raw string as JSON when possible; falls back to a JSON string
/// holding the raw value unchanged. Used where text arrives from outside a
/// typed pipeline (a CLI argument, a test assertion over rendered output)
/// and a best-effort structured view is wanted.
pub(crate) fn parse_input(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
}

/// The text form of a value, shared by every place a typed value becomes
/// text (a user message, a command's stdin, a written file, a bare template
/// placeholder, `lait run`'s final output): a string is used as-is, anything
/// else is rendered as compact JSON.
pub(crate) fn to_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Renders `template` against `input`/`steps`/`vars` in one shot — a
/// convenience wrapper around [`RenderScope`] for a caller that renders
/// only one template against one set of data. See `RenderScope` for what
/// each placeholder resolves to.
pub(crate) fn render(
    template: &str,
    input: &serde_json::Value,
    steps: &serde_json::Map<String, serde_json::Value>,
    vars: &serde_json::Map<String, serde_json::Value>,
) -> Result<String> {
    RenderScope::new(input, steps, vars)?.render(template)
}

/// One `{ input, steps, vars }` rendering context, reusable across every
/// template rendered against the same data (a shell tool's argv list, say).
///
/// `input`, `steps`, and `vars` are exposed as `{{ input }}`,
/// `{{ steps.<id> }}`, and `{{ vars.<key> }}` (a named prompt's `vars:`
/// merged with `--var` overrides; empty for most callers). Referencing an
/// undefined variable is an error rather than an empty string. A bare
/// placeholder renders its value's text form (see [`to_text`]): a string
/// as-is, anything else as compact JSON, never handlebars' `[object]`
/// placeholder. `{{ json x }}` always renders JSON (quoting strings).
///
/// Building the underlying handlebars `Context` converts the data into an
/// owned `serde_json::Value` tree — `handlebars::Context` always owns its
/// data — so this conversion happens once per `RenderScope::new` call, not
/// once per template rendered against it.
pub(crate) struct RenderScope {
    context: handlebars::Context,
}

impl RenderScope {
    pub(crate) fn new(
        input: &serde_json::Value,
        steps: &serde_json::Map<String, serde_json::Value>,
        vars: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self> {
        Self::from_data(&VarsData { input, steps, vars })
    }

    fn from_data<T: Serialize>(data: &T) -> Result<Self> {
        let value = serde_json::to_value(data).context("failed to build template data")?;
        Ok(Self {
            context: handlebars::Context::from(value),
        })
    }

    /// Renders `template` against this scope's data. The template text is
    /// compiled once (cached in `TEMPLATE_CACHE`, keyed by source text) and
    /// reused across every call — including from a different `RenderScope`
    /// — instead of being re-parsed on every render.
    pub(crate) fn render(&self, template: &str) -> Result<String> {
        let cached = compiled_template(template)?;
        // Equivalent to `Handlebars::render_template`'s path for an
        // unregistered (ad-hoc) template: `None` as the root template name
        // (an ad-hoc `Template::compile` result has no name) and the
        // registry's default (unset) `recursive_lookup`, which `HANDLEBARS`
        // never turns on.
        let mut render_context = RenderContext::new(None);
        cached
            .renders(&HANDLEBARS, &self.context, &mut render_context)
            .with_context(|| format!("failed to render template: {template:?}"))
    }
}

/// Renders a workflow/agent template against `input` and the workflow
/// globals: `{{ steps.<id> }}`, `{{ inputs.<name> }}`, and `{{ loop.index }}`/
/// `{{ loop.item }}` — the same data jq filters see as `$steps`/`$inputs`/
/// `$loop` (see [`jq::Globals`]).
pub(crate) fn render_in(
    template: &str,
    input: &serde_json::Value,
    globals: &jq::Globals,
) -> Result<String> {
    RenderScope::from_data(&WorkflowData {
        input,
        steps: &globals.steps,
        inputs: &globals.inputs,
        loop_context: &globals.loop_context,
    })?
    .render(template)
}

/// Renders a template for `lait run --dry-run`, where only `inputs` (and,
/// for the first step, `input`) are known: any reference to `input` when it
/// is `None`, or to `steps`/`loop`, fails the render so the caller can show
/// the template unrendered instead of a misleading value.
pub(crate) fn render_preview(
    template: &str,
    input: Option<&serde_json::Value>,
    inputs: &serde_json::Map<String, serde_json::Value>,
) -> Result<String> {
    #[derive(Serialize)]
    struct PreviewData<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<&'a serde_json::Value>,
        inputs: &'a serde_json::Map<String, serde_json::Value>,
    }
    RenderScope::from_data(&PreviewData { input, inputs })?.render(template)
}

/// The registry every render shares: nothing about it depends on the
/// template or data being rendered (strict mode, no escaping, the `json`/
/// `text` helpers), so it is built once instead of re-registered per call.
static HANDLEBARS: LazyLock<Handlebars<'static>> = LazyLock::new(|| {
    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars.register_helper("json", Box::new(json_helper));
    handlebars.register_helper(TEXT_HELPER, Box::new(text_helper));
    handlebars
});

const TEXT_HELPER: &str = "text";

#[derive(Serialize)]
struct VarsData<'a> {
    input: &'a serde_json::Value,
    steps: &'a serde_json::Map<String, serde_json::Value>,
    vars: &'a serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize)]
struct WorkflowData<'a> {
    input: &'a serde_json::Value,
    steps: &'a serde_json::Map<String, serde_json::Value>,
    inputs: &'a serde_json::Map<String, serde_json::Value>,
    #[serde(rename = "loop")]
    loop_context: &'a serde_json::Value,
}

/// Templates compiled by [`compiled_template`], shared across every render
/// in the process. `Handlebars::render_template` compiles its argument from
/// scratch on every call (there is no built-in cache for an ad-hoc,
/// unregistered template string); caching the compiled result here means a
/// template text rendered more than once — most commonly a `for_each`/
/// `while`/`until` body's `prompt:`/`system:` re-run per iteration — is only
/// parsed (and bare-path-rewritten, see [`rewrite_bare_paths`]) the first
/// time. Deliberately left unbounded, the same way `jq::FILTER_CACHE` is:
/// for a single `lait run`/`lait chat` process, a workflow's set of
/// distinct template strings is fixed at parse time. `lait lint <DIR>` is
/// the one case where this grows across every workflow file in a directory
/// tree rather than one workflow — still bounded by the distinct template
/// strings on disk, and the process exits once linting finishes.
static TEMPLATE_CACHE: LazyLock<SyncCache<Template>> = LazyLock::new(SyncCache::new);

/// Compiles `template` (after [`rewrite_bare_paths`]), or returns the cached
/// result of an earlier call with the same source text.
fn compiled_template(template: &str) -> Result<Arc<Template>> {
    TEMPLATE_CACHE.get_or_init(template, |template| {
        Template::compile(&rewrite_bare_paths(template))
            .with_context(|| format!("failed to parse template: {template:?}"))
    })
}

/// Checks `template`'s handlebars syntax without rendering it (used by the
/// linter, which has no data to render against yet). This only catches
/// malformed `{{ ... }}`/block syntax; a reference to an undefined variable
/// is only ever caught by an actual render. Goes through the same
/// `compiled_template` cache every render uses, so a template the linter
/// already checked doesn't pay the parse cost twice.
pub(crate) fn check_syntax(template: &str) -> Result<()> {
    compiled_template(template).map(|_| ())
}

/// Every `name.path` referenced by a plain `{{ ... }}` expression or helper
/// argument whose root is `root` (e.g. `root = "inputs"` finds `lang` in
/// `{{ inputs.lang }}` and `{{ json inputs.lang }}`). Used by the linter to
/// check references against declared inputs/step ids; only the first path
/// segment after `root` is returned.
pub(crate) fn referenced_fields(template: &str, root: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        for token in after[..end].split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
            if let Some(field) = token
                .strip_prefix(root)
                .and_then(|tail| tail.strip_prefix('.'))
            {
                let name: String = field
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if !name.is_empty() && !found.contains(&name) {
                    found.push(name);
                }
            }
        }
        rest = &after[end + 2..];
    }
    found
}

/// Rewrites every plain `{{ path }}` expression (no helper, block, comment,
/// partial, or triple-stash) into `{{text path}}`, so a bare placeholder
/// renders its value's text form instead of handlebars' `[object]`/`[array]`
/// placeholder for a structured value. Strict mode still applies to the
/// helper's argument, so an undefined path remains an error.
fn rewrite_bare_paths(template: &str) -> Cow<'_, str> {
    if !template.contains("{{") {
        return Cow::Borrowed(template);
    }
    let mut output = String::with_capacity(template.len() + 16);
    let mut rest = template;
    let mut changed = false;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if after.starts_with('{') {
            // Triple-stash: copy through its closing `}}}` unchanged.
            let Some(end) = after.find("}}}") else {
                output.push_str(&rest[start..]);
                return finish(output, changed, template);
            };
            output.push_str(&rest[start..start + 2 + end + 3]);
            rest = &after[end + 3..];
            continue;
        }
        let Some(end) = after.find("}}") else {
            output.push_str(&rest[start..]);
            return finish(output, changed, template);
        };
        let inner = after[..end].trim();
        if is_plain_path(inner) {
            output.push_str("{{");
            output.push_str(TEXT_HELPER);
            output.push(' ');
            output.push_str(inner);
            output.push_str("}}");
            changed = true;
        } else {
            output.push_str(&rest[start..start + 2 + end + 2]);
        }
        rest = &after[end + 2..];
    }
    output.push_str(rest);
    finish(output, changed, template)
}

fn finish(output: String, changed: bool, template: &str) -> Cow<'_, str> {
    if changed {
        Cow::Owned(output)
    } else {
        Cow::Borrowed(template)
    }
}

/// Whether `expression` (the trimmed text between `{{` and `}}`) is a bare
/// path such as `input`, `steps.extract.city`, `this`, `@index`, or
/// `../name` — not a helper call, block, comment, partial, or `else`.
fn is_plain_path(expression: &str) -> bool {
    let Some(first) = expression.chars().next() else {
        return false;
    };
    if !(first.is_alphanumeric() || matches!(first, '_' | '@' | '.')) {
        return false;
    }
    if expression == "else" {
        return false;
    }
    expression
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '@' | '/' | '[' | ']'))
}

/// The first argument of a helper call. Handlebars' strict mode does not
/// cover helper arguments, so a path that resolves to nothing is rejected
/// here, keeping `{{ json steps.missing }}`/`{{ steps.missing }}` as strict
/// as any other reference.
fn strict_param<'a>(
    helper: &'a Helper,
    name: &'static str,
) -> Result<&'a serde_json::Value, RenderErrorReason> {
    let param = helper
        .param(0)
        .ok_or(RenderErrorReason::ParamNotFoundForIndex(name, 0))?;
    if param.is_value_missing() {
        return Err(RenderErrorReason::MissingVariable(
            param.relative_path().cloned(),
        ));
    }
    Ok(param.value())
}

fn json_helper(
    helper: &Helper,
    _: &Handlebars,
    _: &handlebars::Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    let value = strict_param(helper, "json")?;
    out.write(&serde_json::to_string(value).map_err(RenderErrorReason::SerdeError)?)?;
    Ok(())
}

fn text_helper(
    helper: &Helper,
    _: &Handlebars,
    _: &handlebars::Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    let value = strict_param(helper, TEXT_HELPER)?;
    out.write(&to_text(value))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        RenderScope, check_syntax, parse_input, referenced_fields, render, render_in,
        rewrite_bare_paths, to_text,
    };
    use crate::jq::Globals;
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
                &json!("hello"),
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            "summarize: hello"
        );
    }

    #[test]
    fn renders_a_field_from_an_object_input() {
        let input = json!({"city": "Tokyo", "population": 37400000});
        assert_eq!(
            render(
                "city: {{ input.city }} ({{input.population}})",
                &input,
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            "city: Tokyo (37400000)"
        );
    }

    #[test]
    fn renders_no_placeholder_text_unchanged() {
        assert_eq!(
            render("no placeholder here", &json!("x"), &no_steps(), &no_vars()).unwrap(),
            "no placeholder here"
        );
    }

    #[test]
    fn rejects_an_undefined_variable() {
        assert!(
            render(
                "{{ input.nope }}",
                &json!({"a": 1}),
                &no_steps(),
                &no_vars()
            )
            .is_err()
        );
        assert!(render("{{ nope }}", &json!("x"), &no_steps(), &no_vars()).is_err());
    }

    #[test]
    fn rejects_an_unterminated_placeholder() {
        assert!(render("{{ input", &json!("x"), &no_steps(), &no_vars()).is_err());
    }

    #[test]
    fn renders_a_bare_object_or_array_placeholder_as_compact_json() {
        assert_eq!(
            render(
                "{{ input }}",
                &json!({"b": 2, "a": 1}),
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            r#"{"b":2,"a":1}"#
        );
        assert_eq!(
            render(
                "{{input}}",
                &json!(["Tokyo", "Osaka"]),
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            r#"["Tokyo","Osaka"]"#
        );
    }

    #[test]
    fn renders_scalars_in_their_text_form() {
        assert_eq!(
            render(
                "{{ input.n }} {{ input.b }} {{ input.z }}",
                &json!({"n": 1.5, "b": true, "z": null}),
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            "1.5 true null"
        );
    }

    #[test]
    fn the_json_helper_quotes_strings() {
        assert_eq!(
            render("{{ json input }}", &json!("a"), &no_steps(), &no_vars()).unwrap(),
            r#""a""#
        );
        assert_eq!(
            render(
                "{{ json input.address }}",
                &json!({"address": {"city": "Tokyo"}}),
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            r#"{"city":"Tokyo"}"#
        );
    }

    #[test]
    fn block_helpers_and_triple_stash_are_left_alone() {
        let input = json!({"items": ["a", "b"], "flag": true});
        assert_eq!(
            render(
                "{{#each input.items}}[{{ this }}:{{@index}}]{{/each}}{{#if input.flag}}Y{{else}}N{{/if}}{{{ input.items.[0] }}}",
                &input,
                &no_steps(),
                &no_vars()
            )
            .unwrap(),
            "[a:0][b:1]Ya"
        );
    }

    #[test]
    fn renders_a_field_from_a_named_step_output() {
        let mut steps = no_steps();
        steps.insert("extract".to_owned(), json!({"city": "Tokyo"}));
        assert_eq!(
            render(
                "city: {{ steps.extract.city }} / {{ steps.extract }}",
                &json!("x"),
                &steps,
                &no_vars()
            )
            .unwrap(),
            r#"city: Tokyo / {"city":"Tokyo"}"#
        );
    }

    #[test]
    fn renders_a_var_placeholder() {
        let mut vars = no_vars();
        vars.insert("lang".to_owned(), json!("英語"));
        assert_eq!(
            render("{{ vars.lang }}", &json!("x"), &no_steps(), &vars).unwrap(),
            "英語"
        );
    }

    #[test]
    fn rejects_a_reference_to_an_unset_var_or_step() {
        assert!(render("{{ vars.missing }}", &json!("x"), &no_steps(), &no_vars()).is_err());
        assert!(render("{{ steps.missing }}", &json!("x"), &no_steps(), &no_vars()).is_err());
    }

    #[test]
    fn render_in_exposes_inputs_steps_and_loop() {
        let mut globals = Globals::default();
        globals.inputs.insert("lang".to_owned(), json!("ja"));
        globals.steps.insert("a".to_owned(), json!(1));
        globals.loop_context = json!({"index": 0, "item": "x"});
        assert_eq!(
            render_in(
                "{{ inputs.lang }}/{{ steps.a }}/{{ loop.index }}/{{ loop.item }}/{{ input }}",
                &json!("in"),
                &globals
            )
            .unwrap(),
            "ja/1/0/x/in"
        );
    }

    #[test]
    fn render_in_rejects_vars_and_a_missing_input() {
        let globals = Globals::default();
        assert!(render_in("{{ vars.lang }}", &json!("x"), &globals).is_err());
        assert!(render_in("{{ inputs.lang }}", &json!("x"), &globals).is_err());
        assert!(render_in("{{ loop.index }}", &json!("x"), &globals).is_err());
    }

    #[test]
    fn to_text_keeps_strings_raw_and_renders_everything_else_as_json() {
        assert_eq!(to_text(&json!("a\"b")), "a\"b");
        assert_eq!(to_text(&json!(42)), "42");
        assert_eq!(to_text(&json!(null)), "null");
        assert_eq!(to_text(&json!({"a": [1]})), r#"{"a":[1]}"#);
    }

    #[test]
    fn parse_input_falls_back_to_a_plain_string_for_non_json_text() {
        assert_eq!(parse_input("Alice"), json!("Alice"));
        assert_eq!(parse_input("42"), json!(42));
        assert_eq!(parse_input(r#"{"a":1}"#), json!({"a": 1}));
    }

    #[test]
    fn check_syntax_accepts_valid_templates_and_rejects_malformed_ones() {
        assert!(check_syntax("summarize: {{ input.city }}").is_ok());
        assert!(check_syntax("plain text").is_ok());
        assert!(check_syntax("{{ nope }}").is_ok());
        assert!(check_syntax("{{ input").is_err());
    }

    #[test]
    fn rewrite_only_touches_plain_paths() {
        assert_eq!(rewrite_bare_paths("a {{ input }} b"), "a {{text input}} b");
        assert_eq!(rewrite_bare_paths("{{ json input }}"), "{{ json input }}");
        assert_eq!(
            rewrite_bare_paths("{{#if x}}{{else}}{{/if}}"),
            "{{#if x}}{{else}}{{/if}}"
        );
        assert_eq!(rewrite_bare_paths("{{! note }}"), "{{! note }}");
        assert_eq!(rewrite_bare_paths("{{{ raw }}}"), "{{{ raw }}}");
    }

    #[test]
    fn referenced_fields_finds_first_segments_under_a_root() {
        assert_eq!(
            referenced_fields(
                "{{ inputs.lang }} {{ json inputs.items }} {{#if inputs.flag.x}}{{/if}} {{ steps.a }}",
                "inputs"
            ),
            vec!["lang".to_owned(), "items".to_owned(), "flag".to_owned()]
        );
    }

    #[test]
    fn rendering_the_same_template_text_twice_through_the_compiled_cache_agrees() {
        // Exercises `compiled_template`'s cache: the second call hits the
        // cache instead of recompiling, and must still produce the same
        // output as the first.
        let input = json!({"city": "Tokyo"});
        let first = render("city: {{ input.city }}", &input, &no_steps(), &no_vars()).unwrap();
        let second = render("city: {{ input.city }}", &input, &no_steps(), &no_vars()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, "city: Tokyo");
    }

    #[test]
    fn a_render_scope_renders_multiple_templates_against_the_same_data() {
        // `RenderScope` builds its handlebars `Context` once and reuses it
        // across `.render()` calls — check that doesn't leak state between
        // the renders or otherwise corrupt either result.
        let input = json!({"city": "Tokyo", "population": 37400000});
        let scope = RenderScope::new(&input, &no_steps(), &no_vars()).unwrap();
        assert_eq!(scope.render("{{ input.city }}").unwrap(), "Tokyo");
        assert_eq!(scope.render("{{ input.population }}").unwrap(), "37400000");
        // Same template text rendered again through the same scope — the
        // shared `TEMPLATE_CACHE` entry must not carry data between renders.
        assert_eq!(scope.render("{{ input.city }}").unwrap(), "Tokyo");
    }
}
