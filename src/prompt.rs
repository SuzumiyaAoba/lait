//! Named prompt templates (`prompts:` in lait.config.yml, run via
//! `-p`/`--prompt-name <NAME>` or `lait prompt <NAME>` — see
//! `docs/usage/ja/prompts.md`). This module holds the config-only logic
//! (lookup, `--var` merging, template rendering, `lait prompt list`);
//! actually sending the rendered text to the model lives in `app.rs`
//! (`app::run_prompt`), which needs its private request-building machinery.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    config::{self, ConfigFile, PromptDefinition},
    template,
};

/// Parses one `--var KEY=VALUE` argument into its `(key, value)` pair. Shared
/// with `workflow::build_vars` (`lait run --var`), which differs only in how
/// it interprets `value` (JSON-coerced there, kept as a plain string here).
pub(crate) fn parse_var(raw: &str) -> Result<(String, String)> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid '--var {raw}'; expected KEY=VALUE"))?;
    if key.is_empty() {
        bail!("invalid '--var {raw}'; the key must not be empty");
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// Looks up `name` in `file_config.prompts`, failing with a clear error that
/// lists the configured names (when there are any) when it isn't defined.
pub(crate) fn lookup<'a>(name: &str, file_config: &'a ConfigFile) -> Result<&'a PromptDefinition> {
    file_config.prompts.get(name).ok_or_else(|| {
        let mut names: Vec<&str> = file_config.prompts.keys().map(String::as_str).collect();
        names.sort_unstable();
        if names.is_empty() {
            anyhow!(
                "no prompt named '{name}' is configured; add a 'prompts.{name}:' entry to {}",
                config::CONFIG_FILE_NAME
            )
        } else {
            anyhow!(
                "no prompt named '{name}' is configured; configured prompts: {}",
                names.join(", ")
            )
        }
    })
}

/// Builds the `vars` object `definition.template` renders against: its own
/// `vars:` defaults, overridden by `cli_vars` (`--var KEY=VALUE`, parsed and
/// applied in order so a repeated key keeps its last value).
pub(crate) fn build_vars(
    definition: &PromptDefinition,
    cli_vars: &[String],
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut vars: HashMap<String, String> = definition.vars.clone();
    for raw in cli_vars {
        let (key, value) = parse_var(raw)?;
        vars.insert(key, value);
    }
    Ok(vars
        .into_iter()
        .map(|(key, value)| (key, serde_json::Value::String(value)))
        .collect())
}

/// Renders named prompt `name` against `raw_input`, returning `(rendered
/// text, the prompt's own model:, if set)`. The model is returned alongside
/// the text because it participates in the caller's model-resolution
/// precedence (`--model` > this > `default.model`) rather than the
/// text-rendering step itself.
pub(crate) fn render_named(
    name: &str,
    raw_input: &str,
    cli_vars: &[String],
    file_config: &ConfigFile,
) -> Result<(String, Option<String>)> {
    let definition = lookup(name, file_config)?;
    let vars = build_vars(definition, cli_vars)?;
    let input = template::parse_input(raw_input);
    let rendered = template::render(&definition.template, &input, &serde_json::Map::new(), &vars)
        .with_context(|| format!("prompt '{name}'"))?;
    Ok((rendered, definition.model.clone()))
}

/// Runs `lait prompt list`: prints every configured prompt's name and, when
/// set, its own model — a no-network, config-only operation.
pub(crate) fn list(file_config: &ConfigFile) -> Result<()> {
    if file_config.prompts.is_empty() {
        println!(
            "no prompts defined in {}; add a 'prompts:' entry to define one",
            config::CONFIG_FILE_NAME
        );
        return Ok(());
    }
    let mut names: Vec<&String> = file_config.prompts.keys().collect();
    names.sort_unstable();
    for name in names {
        match &file_config.prompts[name].model {
            Some(model) => println!("{name}  (model: {model})"),
            None => println!("{name}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_vars, lookup, render_named};
    use crate::config::{ConfigFile, PromptDefinition};
    use std::collections::HashMap;

    fn config_with(prompts: Vec<(&str, PromptDefinition)>) -> ConfigFile {
        let mut config = ConfigFile::default();
        for (name, definition) in prompts {
            config.prompts.insert(name.to_owned(), definition);
        }
        config
    }

    fn definition(template: &str) -> PromptDefinition {
        PromptDefinition {
            template: template.to_owned(),
            model: None,
            vars: HashMap::new(),
        }
    }

    #[test]
    fn lookup_finds_a_configured_prompt() {
        let config = config_with(vec![("translate", definition("{{ input }}"))]);
        assert!(lookup("translate", &config).is_ok());
    }

    #[test]
    fn lookup_fails_with_the_configured_names_when_missing() {
        let config = config_with(vec![("translate", definition("{{ input }}"))]);
        let error = lookup("nope", &config).unwrap_err();
        assert!(error.to_string().contains("translate"));
    }

    #[test]
    fn lookup_fails_clearly_when_no_prompt_is_configured_at_all() {
        let config = ConfigFile::default();
        let error = lookup("nope", &config).unwrap_err();
        assert!(error.to_string().contains("no prompt named"));
    }

    #[test]
    fn build_vars_uses_the_prompt_defaults_when_no_override_is_given() {
        let mut definition = definition("{{ vars.lang }}");
        definition
            .vars
            .insert("lang".to_owned(), "日本語".to_owned());
        let vars = build_vars(&definition, &[]).unwrap();
        assert_eq!(vars["lang"], "日本語");
    }

    #[test]
    fn build_vars_lets_a_cli_override_win() {
        let mut definition = definition("{{ vars.lang }}");
        definition
            .vars
            .insert("lang".to_owned(), "日本語".to_owned());
        let vars = build_vars(&definition, &["lang=英語".to_owned()]).unwrap();
        assert_eq!(vars["lang"], "英語");
    }

    #[test]
    fn build_vars_rejects_a_malformed_var() {
        let definition = definition("{{ input }}");
        assert!(build_vars(&definition, &["no-equals-sign".to_owned()]).is_err());
    }

    #[test]
    fn render_named_renders_the_template_against_input_and_vars() {
        let mut definition = definition("{{ input }} in {{ vars.lang }}");
        definition.vars.insert("lang".to_owned(), "英語".to_owned());
        definition.model = Some("gpt-oss-20b".to_owned());
        let config = config_with(vec![("translate", definition)]);

        let (rendered, model) = render_named("translate", "Hello", &[], &config).unwrap();
        assert_eq!(rendered, "Hello in 英語");
        assert_eq!(model.as_deref(), Some("gpt-oss-20b"));
    }

    #[test]
    fn render_named_fails_for_an_unknown_prompt() {
        let config = ConfigFile::default();
        assert!(render_named("nope", "Hello", &[], &config).is_err());
    }
}
