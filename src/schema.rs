use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use async_openai::types::chat::{ResponseFormat, ResponseFormatJsonSchema};
use serde::Deserialize;

use crate::async_io;

/// A map of schema name to its definition, as used by a workflow file's
/// top-level `json_schemas:` and an agent file's `input_schema:`/`output_schema:`.
pub(crate) type JsonSchemaMap = HashMap<String, JsonSchemaEntry>;

/// A named schema definition: either a path to a JSON schema file, or the
/// schema body written directly inline.
#[derive(Debug, Clone, Deserialize)]
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

/// Resolves a schema entry through the cancellation-aware filesystem worker
/// used by timed workflow steps. Inline schemas remain an inexpensive clone;
/// file-backed schemas are read in bounded chunks and Unix special files are
/// opened non-blocking by [`async_io::read_file`].
pub(crate) async fn load_schema_value_cancellable(
    entry: &JsonSchemaEntry,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<serde_json::Value> {
    match entry {
        JsonSchemaEntry::Inline { schema } => Ok(schema.clone()),
        JsonSchemaEntry::FilePath { file_path } => {
            let contents = async_io::read_to_string_cancellable(
                file_path,
                cancellation,
                async_io::MAX_READ_BYTES,
            )
            .await
            .with_context(|| {
                format!("failed to read JSON schema file '{}'", file_path.display())
            })?;
            serde_json::from_str(&contents).with_context(|| {
                format!("failed to parse JSON schema file '{}'", file_path.display())
            })
        }
    }
}

/// Resolves an entry to a Structured Outputs `response_format`, under `name`,
/// while allowing timed workflow steps to cancel file-backed schema reads.
pub(crate) async fn build_response_format_from_entry_cancellable(
    entry: &JsonSchemaEntry,
    name: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<ResponseFormat> {
    build_json_schema(
        load_schema_value_cancellable(entry, cancellation).await?,
        name,
    )
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

/// Cancellation-aware counterpart to [`resolve_named_schema_value`], used for
/// a workflow node's `input_schema` before its model call starts.
pub(crate) async fn resolve_named_schema_value_cancellable(
    json_schemas: &JsonSchemaMap,
    name_or_path: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<serde_json::Value> {
    match json_schemas.get(name_or_path) {
        Some(entry) => load_schema_value_cancellable(entry, cancellation).await,
        None => {
            let path = PathBuf::from(name_or_path);
            let contents =
                async_io::read_to_string_cancellable(&path, cancellation, async_io::MAX_READ_BYTES)
                    .await
                    .with_context(|| format!("failed to read JSON schema file '{name_or_path}'"))?;
            serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse JSON schema file '{}'", path.display()))
        }
    }
}

/// Checks `input` against `schema` well enough to catch the common mistakes:
/// the top level must be a JSON object, and then — recursively, through
/// `properties`/`items` — every value present is checked against its
/// sub-schema's `type` (including a JSON Schema array-of-types) and `enum`,
/// and every object checked against its sub-schema's `required`. This still
/// isn't full JSON Schema validation: `format`, `pattern`, numeric bounds,
/// `additionalProperties`, `oneOf`/`anyOf`/`allOf`, and `$ref` are not
/// checked, and a field the schema doesn't mention is never rejected (a
/// schema written for a Structured Outputs `output_schema` — which requires
/// `additionalProperties: false` in strict mode — must stay reusable as an
/// `input_schema` without also rejecting extra input fields). Just enough to
/// fail fast with a clear message before a template silently renders a hole
/// where a field should be, or a request is sent with a field of the wrong
/// shape.
pub(crate) fn validate_input_against_schema(
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<()> {
    if !input.is_object() {
        bail!("input must be a JSON object matching the input schema");
    }
    validate_value_against_schema(schema, input, "input")
}

/// The JSON Schema `type` keyword's name for `value`'s own runtime type, used
/// only to report a mismatch; `"integer"` is JSON Schema's term for a number
/// with no fractional part, so a whole-number `serde_json::Value::Number` is
/// reported as `"number"` here (the type it always satisfies) even though it
/// would also satisfy a schema declaring `"integer"`.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// JSON Schema's seven primitive `type` keyword names. Kept alongside
/// `matches_json_type`'s match arms (each name below has one there) so
/// [`unrecognized_type_names`] can tell `lint` about a name neither
/// recognizes, such as a typo (`type: sting`) that would otherwise silently
/// match any value.
const RECOGNIZED_JSON_SCHEMA_TYPES: &[&str] = &[
    "object", "array", "string", "boolean", "null", "integer", "number",
];

/// Whether `value` satisfies a single JSON Schema `type` keyword value (e.g.
/// `"string"`, or `"integer"` for a number with no fractional part). An
/// unrecognized type name is treated as satisfied by anything, the same way
/// an unrecognized schema keyword elsewhere is silently ignored rather than
/// rejected — `lint` surfaces this case separately (see
/// [`unrecognized_type_names`]) since it can't be caught here without
/// turning every request into a hard failure over what might be a typo.
fn matches_json_type(type_name: &str, value: &serde_json::Value) -> bool {
    match type_name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "number" => value.is_number(),
        _ => true,
    }
}

/// Collects every distinct `type` keyword value found anywhere in `schema`
/// (recursing through `properties`/`items`, the same nesting
/// `validate_value_against_schema` walks) that isn't one of JSON Schema's
/// recognized primitive type names — for `lint` to warn about, since
/// `matches_json_type` otherwise treats such a name as matching any value
/// without any indication why a field went unchecked.
pub(crate) fn unrecognized_type_names(schema: &serde_json::Value) -> Vec<String> {
    let mut found = Vec::new();
    collect_unrecognized_type_names(schema, &mut found);
    found
}

fn collect_unrecognized_type_names(schema: &serde_json::Value, found: &mut Vec<String>) {
    if let Some(type_value) = schema.get("type") {
        let names: Vec<&str> = match type_value {
            serde_json::Value::String(name) => vec![name.as_str()],
            serde_json::Value::Array(names) => {
                names.iter().filter_map(|name| name.as_str()).collect()
            }
            _ => Vec::new(),
        };
        for name in names {
            if !RECOGNIZED_JSON_SCHEMA_TYPES.contains(&name) && !found.iter().any(|f| f == name) {
                found.push(name.to_owned());
            }
        }
    }
    if let Some(properties) = schema.get("properties").and_then(|value| value.as_object()) {
        for property_schema in properties.values() {
            collect_unrecognized_type_names(property_schema, found);
        }
    }
    if let Some(items_schema) = schema.get("items") {
        collect_unrecognized_type_names(items_schema, found);
    }
}

/// Recursively checks `value` (found at `path`, used only to name the field
/// in an error message) against `schema`'s `type`/`enum`/`required`/
/// `properties`/`items` keywords. Any keyword `schema` doesn't set is
/// skipped, so a schema that only declares `required` behaves exactly as
/// before this function grew type/nesting checks.
fn validate_value_against_schema(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
) -> Result<()> {
    if let Some(type_value) = schema.get("type") {
        let allowed_types: Vec<&str> = match type_value {
            serde_json::Value::String(name) => vec![name.as_str()],
            serde_json::Value::Array(names) => {
                names.iter().filter_map(|name| name.as_str()).collect()
            }
            _ => Vec::new(),
        };
        if !allowed_types.is_empty()
            && !allowed_types
                .iter()
                .any(|type_name| matches_json_type(type_name, value))
        {
            bail!(
                "{path} must be of type {} (got {})",
                allowed_types.join(" or "),
                json_type_name(value)
            );
        }
    }

    if let Some(allowed_values) = schema.get("enum").and_then(|value| value.as_array())
        && !allowed_values.contains(value)
    {
        bail!("{path} must be one of {allowed_values:?} (got {value})");
    }

    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(|value| value.as_array()) {
            let missing: Vec<&str> = required
                .iter()
                .filter_map(|key| key.as_str())
                .filter(|key| !object.contains_key(*key))
                .collect();
            if !missing.is_empty() {
                bail!(
                    "{path} is missing required field(s): {}",
                    missing.join(", ")
                );
            }
        }
        if let Some(properties) = schema.get("properties").and_then(|value| value.as_object()) {
            for (key, property_schema) in properties {
                if let Some(property_value) = object.get(key) {
                    validate_value_against_schema(
                        property_schema,
                        property_value,
                        &format!("{path}.{key}"),
                    )?;
                }
            }
        }
    }

    if let Some(items_schema) = schema.get("items")
        && let Some(array) = value.as_array()
    {
        for (index, item) in array.iter().enumerate() {
            validate_value_against_schema(items_schema, item, &format!("{path}[{index}]"))?;
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

/// Cancellation-aware counterpart to [`load_json_schema`], used for a
/// workflow node's file-backed `output_schema`.
pub(crate) async fn load_json_schema_cancellable(
    path: &Path,
    name: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<ResponseFormat> {
    let contents =
        async_io::read_to_string_cancellable(path, cancellation, async_io::MAX_READ_BYTES)
            .await
            .with_context(|| format!("failed to read JSON schema file '{}'", path.display()))?;
    let schema = serde_json::from_str::<serde_json::Value>(&contents)
        .with_context(|| format!("failed to parse JSON schema file '{}'", path.display()))?;
    build_json_schema(schema, name)
}

/// Checks a Structured Outputs schema `name` (a node/agent's `schema_name`,
/// defaulting to `"structured_output"`) against the constraints
/// `build_json_schema` requires but which nothing checks before request time:
/// 1-64 characters, ASCII letters/digits/underscore/hyphen only. Exposed on
/// its own (rather than folded back into `build_json_schema`, its only
/// caller before the linter) so the linter can validate a `schema_name` it
/// finds statically without also needing the schema body on hand.
pub(crate) fn validate_schema_name(name: &str) -> Result<()> {
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
    Ok(())
}

pub(crate) fn build_json_schema(schema: serde_json::Value, name: &str) -> Result<ResponseFormat> {
    validate_schema_name(name)?;

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
    use super::{unrecognized_type_names, validate_input_against_schema};
    use serde_json::json;

    #[test]
    fn finds_no_unrecognized_types_in_an_ordinary_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "age": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "string"}},
            },
        });
        assert!(unrecognized_type_names(&schema).is_empty());
    }

    #[test]
    fn finds_an_unrecognized_top_level_type() {
        let schema = json!({"type": "sting"});
        assert_eq!(unrecognized_type_names(&schema), vec!["sting".to_owned()]);
    }

    #[test]
    fn finds_an_unrecognized_type_nested_in_properties_and_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "age": {"type": "int"},
                "tags": {"type": "array", "items": {"type": "txt"}},
            },
        });
        let mut found = unrecognized_type_names(&schema);
        found.sort();
        assert_eq!(found, vec!["int".to_owned(), "txt".to_owned()]);
    }

    #[test]
    fn finds_an_unrecognized_type_inside_an_array_of_types() {
        let schema = json!({"type": ["string", "nullish"]});
        assert_eq!(unrecognized_type_names(&schema), vec!["nullish".to_owned()]);
    }

    #[test]
    fn deduplicates_a_repeated_unrecognized_type_name() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {"type": "sting"},
                "b": {"type": "sting"},
            },
        });
        assert_eq!(unrecognized_type_names(&schema), vec!["sting".to_owned()]);
    }

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

    #[test]
    fn accepts_a_property_of_the_declared_type() {
        let schema = json!({
            "type": "object",
            "properties": {"age": {"type": "integer"}},
        });
        assert!(validate_input_against_schema(&schema, &json!({"age": 30})).is_ok());
    }

    #[test]
    fn rejects_a_property_of_the_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": {"age": {"type": "integer"}},
        });
        let error = validate_input_against_schema(&schema, &json!({"age": "thirty"})).unwrap_err();
        assert!(error.to_string().contains("input.age"), "{error}");
        assert!(error.to_string().contains("integer"), "{error}");
    }

    #[test]
    fn rejects_a_non_integer_number_for_an_integer_property() {
        let schema = json!({
            "type": "object",
            "properties": {"age": {"type": "integer"}},
        });
        assert!(validate_input_against_schema(&schema, &json!({"age": 30.5})).is_err());
    }

    #[test]
    fn accepts_either_type_in_an_array_of_types() {
        let schema = json!({
            "type": "object",
            "properties": {"nickname": {"type": ["string", "null"]}},
        });
        assert!(validate_input_against_schema(&schema, &json!({"nickname": "Taro"})).is_ok());
        assert!(validate_input_against_schema(&schema, &json!({"nickname": null})).is_ok());
        assert!(validate_input_against_schema(&schema, &json!({"nickname": 1})).is_err());
    }

    #[test]
    fn validates_a_nested_object_property() {
        let schema = json!({
            "type": "object",
            "properties": {
                "address": {
                    "type": "object",
                    "required": ["city"],
                    "properties": {"city": {"type": "string"}},
                },
            },
        });
        assert!(
            validate_input_against_schema(&schema, &json!({"address": {"city": "Tokyo"}})).is_ok()
        );

        let error = validate_input_against_schema(&schema, &json!({"address": {}})).unwrap_err();
        assert!(error.to_string().contains("input.address"), "{error}");
        assert!(error.to_string().contains("city"), "{error}");

        let error =
            validate_input_against_schema(&schema, &json!({"address": {"city": 1}})).unwrap_err();
        assert!(error.to_string().contains("input.address.city"), "{error}");
    }

    #[test]
    fn validates_array_items_against_their_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"tags": {"type": "array", "items": {"type": "string"}}},
        });
        assert!(validate_input_against_schema(&schema, &json!({"tags": ["a", "b"]})).is_ok());

        let error = validate_input_against_schema(&schema, &json!({"tags": ["a", 1]})).unwrap_err();
        assert!(error.to_string().contains("input.tags[1]"), "{error}");
    }

    #[test]
    fn rejects_a_value_outside_an_enum() {
        let schema = json!({
            "type": "object",
            "properties": {"status": {"enum": ["open", "closed"]}},
        });
        assert!(validate_input_against_schema(&schema, &json!({"status": "open"})).is_ok());
        assert!(validate_input_against_schema(&schema, &json!({"status": "pending"})).is_err());
    }

    #[test]
    fn ignores_extra_fields_not_declared_in_properties() {
        // A schema written for a Structured Outputs `output_schema` (strict
        // mode requires `additionalProperties: false`) must stay usable as an
        // `input_schema` without rejecting extra input fields.
        let schema = json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "additionalProperties": false,
        });
        assert!(
            validate_input_against_schema(&schema, &json!({"city": "Tokyo", "extra": 1})).is_ok()
        );
    }
}
