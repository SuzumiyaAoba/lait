use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ResponseFormat, ResponseFormatJsonSchema};
use serde::Deserialize;

/// A map of schema name to its definition, as used by a workflow file's
/// top-level `json_schemas:` and an agent file's `input_schema:`/`output_schema:`.
pub(crate) type JsonSchemaMap = HashMap<String, JsonSchemaEntry>;

/// A named schema definition: either a path to a JSON schema file, or the
/// schema body written directly inline.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub(crate) enum JsonSchemaEntry {
    FilePath { file_path: PathBuf },
    Inline { schema: serde_json::Value },
}

/// Resolves an entry to its JSON Schema body, reading the file for a
/// `FilePath` entry.
pub(crate) fn load_schema_value(entry: &JsonSchemaEntry) -> Result<serde_json::Value> {
    match entry {
        JsonSchemaEntry::Inline { schema } => Ok(schema.clone()),
        JsonSchemaEntry::FilePath { file_path } => {
            let contents = fs::read_to_string(file_path).with_context(|| {
                format!("failed to read JSON schema file '{}'", file_path.display())
            })?;
            serde_json::from_str(&contents).with_context(|| {
                format!("failed to parse JSON schema file '{}'", file_path.display())
            })
        }
    }
}

/// Resolves an entry to a Structured Outputs `response_format`, under `name`.
pub(crate) fn build_response_format_from_entry(
    entry: &JsonSchemaEntry,
    name: &str,
) -> Result<ResponseFormat> {
    build_json_schema(load_schema_value(entry)?, name)
}

/// Resolves a `StepDefinition::input_schema` value to its schema body: first
/// as a key into a workflow's `json_schemas:`, falling back to treating it as
/// a path to a JSON schema file (the same two-step lookup `json_schema` uses
/// for output schemas).
pub(crate) fn resolve_named_schema_value(
    json_schemas: &JsonSchemaMap,
    name_or_path: &str,
) -> Result<serde_json::Value> {
    match json_schemas.get(name_or_path) {
        Some(entry) => load_schema_value(entry),
        None => {
            let contents = fs::read_to_string(name_or_path)
                .with_context(|| format!("failed to read JSON schema file '{name_or_path}'"))?;
            serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse JSON schema file '{name_or_path}'"))
        }
    }
}

/// Checks `input` against `schema` well enough to catch the common mistakes: it
/// must be a JSON object, and it must have every key `schema.required` lists.
/// This is not full JSON Schema validation (types, formats, nested schemas are
/// not checked) — just enough to fail fast with a clear message before a
/// template silently renders a hole where a field should be.
pub(crate) fn validate_input_against_schema(
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<()> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("input must be a JSON object matching the input schema"))?;
    if let Some(required) = schema.get("required").and_then(|value| value.as_array()) {
        let missing: Vec<&str> = required
            .iter()
            .filter_map(|key| key.as_str())
            .filter(|key| !object.contains_key(*key))
            .collect();
        if !missing.is_empty() {
            bail!("input is missing required field(s): {}", missing.join(", "));
        }
    }
    Ok(())
}

pub(crate) fn load_json_schema(path: &Path, name: &str) -> Result<ResponseFormat> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read JSON schema file '{}'", path.display()))?;
    let schema = serde_json::from_str::<serde_json::Value>(&contents)
        .with_context(|| format!("failed to parse JSON schema file '{}'", path.display()))?;
    build_json_schema(schema, name)
}

pub(crate) fn build_json_schema(schema: serde_json::Value, name: &str) -> Result<ResponseFormat> {
    if name.is_empty() || name.len() > 64 {
        bail!("JSON schema name must be between 1 and 64 characters: {name:?}");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!(
            "JSON schema name must contain only ASCII letters, digits, underscores, or hyphens: {name:?}"
        );
    }

    Ok(ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: None,
            name: name.to_owned(),
            schema,
            strict: Some(true),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::validate_input_against_schema;
    use serde_json::json;

    #[test]
    fn accepts_an_object_with_every_required_field() {
        let schema = json!({"type": "object", "required": ["city"]});
        let input = json!({"city": "Tokyo", "extra": true});
        assert!(validate_input_against_schema(&schema, &input).is_ok());
    }

    #[test]
    fn rejects_a_non_object_input() {
        let schema = json!({"type": "object", "required": ["city"]});
        assert!(validate_input_against_schema(&schema, &json!("Tokyo")).is_err());
    }

    #[test]
    fn rejects_an_object_missing_a_required_field() {
        let schema = json!({"type": "object", "required": ["city", "population"]});
        let input = json!({"city": "Tokyo"});
        let error = validate_input_against_schema(&schema, &input).unwrap_err();
        assert!(error.to_string().contains("population"));
    }

    #[test]
    fn accepts_any_object_when_the_schema_has_no_required_list() {
        let schema = json!({"type": "object"});
        assert!(validate_input_against_schema(&schema, &json!({})).is_ok());
    }
}
