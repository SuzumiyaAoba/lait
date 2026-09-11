//! JSON Schema loading and validation: a workflow's top-level `json_schemas:`
//! map ([`JsonSchemaMap`]/[`JsonSchemaEntry`]), `--json-schema`/`response_format:`
//! resolution into an OpenAI-compatible [`ResponseFormat`], and `lait schema`
//! itself ([`run`]). Every load path exists in a sync and a `_cancellable`
//! async twin (`load_schema_value`/`_cancellable`, `resolve_named_schema_value`/
//! `_cancellable`, `load_json_schema`/`_cancellable`) sharing a pure parsing
//! core (`parse_schema_entry_contents`/`parse_json_schema_contents`) — only
//! the read (`async_io::read_to_string_sync` vs. the cancellable worker)
//! differs between them.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use async_openai::types::chat::{ResponseFormat, ResponseFormatJsonSchema};
use serde::Deserialize;

use crate::{
    async_io,
    cli::{SchemaArgs, SchemaKind},
};

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

/// The "failed to read/parse JSON schema file '{}'" messages this module
/// repeats at every one of its (sync, `_cancellable`) read pairs — some
/// identify the file by `Path::display()`, others by an already-resolved
/// `name_or_path: &str` (see [`resolve_named_schema_value`]), hence
/// `impl Display` rather than `&Path` specifically.
fn read_schema_context(path_or_name: impl std::fmt::Display) -> String {
    format!("failed to read JSON schema file '{path_or_name}'")
}
fn parse_schema_context(path_or_name: impl std::fmt::Display) -> String {
    format!("failed to parse JSON schema file '{path_or_name}'")
}

/// The pure part of resolving a file-backed schema entry, shared by
/// [`load_schema_value`]/[`load_schema_value_cancellable`]: only the read
/// (`async_io::read_to_string_sync` vs. the cancellation-aware worker)
/// differs between them.
fn parse_schema_entry_contents(contents: &str, file_path: &Path) -> Result<serde_json::Value> {
    serde_json::from_str(contents).with_context(|| parse_schema_context(file_path.display()))
}

/// Resolves an entry to its JSON Schema body, reading the file for a
/// `FilePath` entry.
pub(crate) fn load_schema_value(entry: &JsonSchemaEntry) -> Result<serde_json::Value> {
    match entry {
        JsonSchemaEntry::Inline { schema } => Ok(schema.clone()),
        JsonSchemaEntry::FilePath { file_path } => {
            let contents = async_io::read_to_string_sync(file_path)
                .with_context(|| read_schema_context(file_path.display()))?;
            parse_schema_entry_contents(&contents, file_path)
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
            .with_context(|| read_schema_context(file_path.display()))?;
            parse_schema_entry_contents(&contents, file_path)
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
            let path = Path::new(name_or_path);
            let contents = async_io::read_to_string_sync(path)
                .with_context(|| read_schema_context(name_or_path))?;
            parse_schema_entry_contents(&contents, path)
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
                    .with_context(|| read_schema_context(name_or_path))?;
            parse_schema_entry_contents(&contents, &path)
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

/// The pure part of loading a file-backed Structured Outputs schema, shared
/// by [`load_json_schema`]/[`load_json_schema_cancellable`]: only the read
/// differs between them.
fn parse_json_schema_contents(contents: &str, path: &Path, name: &str) -> Result<ResponseFormat> {
    let schema = serde_json::from_str::<serde_json::Value>(contents)
        .with_context(|| parse_schema_context(path.display()))?;
    build_json_schema(schema, name)
}

pub(crate) fn load_json_schema(path: &Path, name: &str) -> Result<ResponseFormat> {
    let contents =
        async_io::read_to_string_sync(path).with_context(|| read_schema_context(path.display()))?;
    parse_json_schema_contents(&contents, path, name)
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
            .with_context(|| read_schema_context(path.display()))?;
    parse_json_schema_contents(&contents, path, name)
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

/// The hand-maintained JSON Schema (draft 2020-12) documents `lait schema`
/// prints, embedded at build time from `schemas/`. Kept hand-written rather
/// than derived (e.g. via `schemars`) since `config::ConfigFile`/
/// `workflow::model::WorkflowFile` lean heavily on `#[serde(deny_unknown_fields)]`,
/// `#[serde(untagged)]`, and per-variant structs that don't map cleanly onto a
/// derive macro; see this module's tests for the cross-check against the real
/// parsers that keeps these from silently drifting.
fn embedded_schema_source(kind: SchemaKind) -> &'static str {
    match kind {
        SchemaKind::Workflow => include_str!("../schemas/workflow.json"),
        SchemaKind::Config => include_str!("../schemas/config.json"),
        SchemaKind::Agent => include_str!("../schemas/agent.json"),
    }
}

/// Parses `kind`'s embedded schema document and re-renders it pretty-printed,
/// which doubles as a self-check that the committed `schemas/*.json` file is
/// itself well-formed JSON.
pub(crate) fn document_schema_json(kind: SchemaKind) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(embedded_schema_source(kind))
        .context("internal error: embedded JSON Schema failed to parse")?;
    serde_json::to_string_pretty(&value).context("failed to render JSON Schema")
}

/// Runs `lait schema workflow|config|agent`: prints the requested document's
/// JSON Schema to stdout. Purely local — see `app::needs_async_runtime`.
pub(crate) fn run(args: SchemaArgs) -> Result<()> {
    println!("{}", document_schema_json(args.kind)?);
    Ok(())
}

#[cfg(test)]
mod tests;
