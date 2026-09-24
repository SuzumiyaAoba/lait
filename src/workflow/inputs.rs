//! Binds a workflow's declared `inputs:` to the values a caller provides —
//! `lait run --input KEY=VALUE`, a `workflow:` step's `with:`, or a test
//! definition's `inputs:` — applying defaults and validating every value
//! against its schema before the first step runs.

use anyhow::{Context, Result, anyhow, bail};

use crate::{schema, template};

use super::model::{InputDefinition, WorkflowFile};

/// The initial value handed to a workflow: text from the command line (or a
/// `lait eval` case), or an already-typed value (a caller's step input, a
/// test definition's `input`).
pub(crate) enum InitialInput {
    Text(String),
    Value(serde_json::Value),
}

/// Resolves a workflow's initial value against its `input_schema`: text is
/// kept as a string unless the schema declares a non-string `type`, in which
/// case it must be JSON (a schema without `type` parses JSON when possible);
/// the result is then validated against the schema.
pub(crate) async fn resolve_initial(
    wf: &WorkflowFile,
    initial: InitialInput,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value> {
    let Some(input_schema) = &wf.input_schema else {
        return Ok(match initial {
            InitialInput::Text(text) => serde_json::Value::String(text),
            InitialInput::Value(value) => value,
        });
    };
    let schema = schema::load_schema_value_cancellable(&input_schema.source, cancellation)
        .await
        .context("the workflow's 'input_schema'")?;
    let value = match initial {
        InitialInput::Value(value) => value,
        InitialInput::Text(text) => match schema.get("type").and_then(serde_json::Value::as_str) {
            Some("string") => serde_json::Value::String(text),
            Some(_) => serde_json::from_str(&text).with_context(
                || "the initial input must be JSON matching the workflow's 'input_schema'",
            )?,
            None => template::parse_input(&text),
        },
    };
    schema::validate_value(&schema, &value, "the initial input")?;
    Ok(value)
}

/// A provided input value: already typed (from `with:`/a test definition),
/// or raw text from the command line, parsed according to its schema.
pub(crate) enum ProvidedInput {
    Value(serde_json::Value),
    Text(String),
}

/// Parses `--input KEY=VALUE` arguments. A later occurrence of the same key
/// wins.
pub(crate) fn parse_cli_inputs(raw: &[String]) -> Result<Vec<(String, ProvidedInput)>> {
    let mut provided: Vec<(String, ProvidedInput)> = Vec::new();
    for argument in raw {
        let (key, value) = argument
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --input {argument:?}: expected KEY=VALUE"))?;
        if key.is_empty() {
            bail!("invalid --input {argument:?}: the key is empty");
        }
        provided.retain(|(existing, _)| existing != key);
        provided.push((key.to_owned(), ProvidedInput::Text(value.to_owned())));
    }
    Ok(provided)
}

/// Converts a typed object (a `with:` result or a test definition's
/// `inputs:`) into provided inputs.
pub(crate) fn from_object(
    object: serde_json::Map<String, serde_json::Value>,
) -> Vec<(String, ProvidedInput)> {
    object
        .into_iter()
        .map(|(key, value)| (key, ProvidedInput::Value(value)))
        .collect()
}

/// Resolves every declared input: a provided value (text parsed according
/// to the schema — kept verbatim for `type: string`, else parsed as JSON
/// when possible), else its `default`, else an error. Undeclared keys are
/// rejected so a typo cannot silently fall back to a default.
pub(crate) fn resolve(
    declared: &[(String, InputDefinition)],
    provided: Vec<(String, ProvidedInput)>,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    for (key, _) in &provided {
        if !declared.iter().any(|(name, _)| name == key) {
            if declared.is_empty() {
                bail!("input '{key}' was provided, but the workflow declares no 'inputs:'");
            }
            let names: Vec<&str> = declared.iter().map(|(name, _)| name.as_str()).collect();
            bail!(
                "unknown input '{key}'; the workflow declares: {}",
                names.join(", ")
            );
        }
    }
    let mut provided = provided;
    let mut resolved = serde_json::Map::new();
    for (name, definition) in declared {
        let value = match provided.iter().position(|(key, _)| key == name) {
            Some(position) => match provided.swap_remove(position).1 {
                ProvidedInput::Value(value) => value,
                ProvidedInput::Text(text) if definition.wants_raw_string() => {
                    serde_json::Value::String(text)
                }
                ProvidedInput::Text(text) => template::parse_input(&text),
            },
            None => match &definition.default {
                Some(default) => default.clone(),
                None => bail!(
                    "missing required input '{name}'{}",
                    definition
                        .description
                        .as_deref()
                        .map(|description| format!(" ({description})"))
                        .unwrap_or_default()
                ),
            },
        };
        schema::validate_value(&definition.schema, &value, &format!("inputs.{name}"))?;
        resolved.insert(name.clone(), value);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::{ProvidedInput, parse_cli_inputs, resolve};
    use crate::workflow::model::InputDefinition;
    use serde_json::json;

    fn declared(entries: &[(&str, serde_json::Value)]) -> Vec<(String, InputDefinition)> {
        entries
            .iter()
            .map(|(name, schema)| {
                (
                    (*name).to_owned(),
                    InputDefinition {
                        default: schema.get("default").cloned(),
                        description: None,
                        schema: schema.clone(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn keeps_text_verbatim_for_a_string_input_and_parses_others() {
        let declared = declared(&[
            ("id", json!({"type": "string"})),
            ("count", json!({"type": "integer"})),
            ("tags", json!({"type": "array"})),
        ]);
        let resolved = resolve(
            &declared,
            parse_cli_inputs(&[
                "id=0012".to_owned(),
                "count=3".to_owned(),
                "tags=[\"a\"]".to_owned(),
            ])
            .unwrap(),
        )
        .unwrap();
        assert_eq!(resolved["id"], json!("0012"));
        assert_eq!(resolved["count"], json!(3));
        assert_eq!(resolved["tags"], json!(["a"]));
    }

    #[test]
    fn applies_defaults_and_requires_the_rest() {
        let declared = declared(&[
            ("lang", json!({"type": "string", "default": "ja"})),
            ("text", json!({"type": "string"})),
        ]);
        let error = resolve(&declared, Vec::new()).unwrap_err();
        assert!(error.to_string().contains("missing required input 'text'"));

        let resolved = resolve(
            &declared,
            vec![("text".to_owned(), ProvidedInput::Value(json!("hi")))],
        )
        .unwrap();
        assert_eq!(resolved["lang"], json!("ja"));
    }

    #[test]
    fn rejects_unknown_and_mistyped_inputs() {
        let declared = declared(&[("count", json!({"type": "integer"}))]);
        assert!(resolve(&declared, parse_cli_inputs(&["cnt=1".to_owned()]).unwrap()).is_err());
        assert!(
            resolve(
                &declared,
                parse_cli_inputs(&["count=many".to_owned()]).unwrap()
            )
            .is_err()
        );
        assert!(resolve(&[], parse_cli_inputs(&["x=1".to_owned()]).unwrap()).is_err());
    }

    #[test]
    fn a_later_cli_value_wins_and_malformed_arguments_are_rejected() {
        let declared = declared(&[("lang", json!({"type": "string"}))]);
        let resolved = resolve(
            &declared,
            parse_cli_inputs(&["lang=en".to_owned(), "lang=fr".to_owned()]).unwrap(),
        )
        .unwrap();
        assert_eq!(resolved["lang"], json!("fr"));
        assert!(parse_cli_inputs(&["novalue".to_owned()]).is_err());
        assert!(parse_cli_inputs(&["=x".to_owned()]).is_err());
    }
}
