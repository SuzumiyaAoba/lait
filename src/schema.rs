//! JSON Schema loading and validation: [`SchemaSource`] (an inline schema or
//! a `{file: ...}` reference, used by a workflow's `schemas:`, a step's
//! `input_schema`/`output_schema`, and an agent's), `--json-schema`/
//! `response_format:` resolution into an OpenAI-compatible
//! [`ResponseFormat`], and `lait schema` itself ([`run`]).
//! `load_schema_value` (used by the sync `lait lint`/agent-loading paths)
//! and `load_schema_value_cancellable` share a pure parsing core
//! (`parse_schema_file_contents`); `load_json_schema_cancellable` (used by
//! every request path, all of them already async) has only the async form,
//! sharing its own pure parsing core (`parse_json_schema_contents`).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use async_openai::types::chat::{ResponseFormat, ResponseFormatJsonSchema};
use serde::{Deserialize, Deserializer};

use crate::{
    async_io,
    cli::{SchemaArgs, SchemaKind},
};

/// Where a JSON Schema body comes from: written inline, or `{file: path}`
/// pointing at a JSON file. Used by a workflow's `schemas:` entries, a
/// step's inline `input_schema`/`output_schema`, and an agent's
/// `input_schema`/`output_schema`.
///
/// A mapping whose only key is `file` (with a string value) is a file
/// reference; any other mapping is the schema itself. `file` is not a JSON
/// Schema keyword, so the two shapes cannot be confused.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SchemaSource {
    Inline(serde_json::Value),
    File(PathBuf),
}

impl<'de> Deserialize<'de> for SchemaSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(serde::de::Error::custom)
    }
}

impl SchemaSource {
    pub(crate) fn from_value(value: serde_json::Value) -> Result<Self, String> {
        match value {
            serde_json::Value::Object(map) => {
                if map.len() == 1
                    && let Some(file) = map.get("file")
                {
                    return match file {
                        serde_json::Value::String(path) if !path.is_empty() => {
                            Ok(Self::File(PathBuf::from(path)))
                        }
                        _ => Err("'file' must be a non-empty path string".to_owned()),
                    };
                }
                Ok(Self::Inline(serde_json::Value::Object(map)))
            }
            serde_json::Value::Bool(_) => Ok(Self::Inline(value)),
            other => Err(format!(
                "expected a JSON Schema object or '{{file: <path>}}', got {other}"
            )),
        }
    }

    /// Resolves a relative `File` path against `base_dir` (the directory of
    /// the workflow/agent file that wrote it). Inline schemas and absolute
    /// paths are unchanged.
    pub(crate) fn resolve_relative_to(&mut self, base_dir: &Path) {
        if let Self::File(path) = self
            && path.is_relative()
        {
            *path = base_dir.join(&*path);
        }
    }

    /// A short description for dry-run/lint output.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Inline(_) => "inline schema".to_owned(),
            Self::File(path) => format!("file '{}'", path.display()),
        }
    }
}

/// A workflow's top-level `schemas:` map, keyed by the name steps refer to.
/// Ordered so lint/dry-run output is stable.
pub(crate) type SchemaMap = BTreeMap<String, SchemaSource>;

/// The "failed to read/parse JSON schema file '{}'" messages this module
/// repeats at every one of its (sync, `_cancellable`) read pairs.
fn read_schema_context(path: impl std::fmt::Display) -> String {
    format!("failed to read JSON schema file '{path}'")
}
fn parse_schema_context(path: impl std::fmt::Display) -> String {
    format!("failed to parse JSON schema file '{path}'")
}

/// The pure part of resolving a file-backed schema source, shared by
/// [`load_schema_value`]/[`load_schema_value_cancellable`]: only the read
/// (`async_io::read_to_string_sync` vs. the cancellation-aware worker)
/// differs between them.
fn parse_schema_file_contents(contents: &str, path: &Path) -> Result<serde_json::Value> {
    serde_json::from_str(contents).with_context(|| parse_schema_context(path.display()))
}

/// Resolves a source to its JSON Schema body, reading the file for a `File`
/// source.
pub(crate) fn load_schema_value(source: &SchemaSource) -> Result<serde_json::Value> {
    match source {
        SchemaSource::Inline(schema) => Ok(schema.clone()),
        SchemaSource::File(path) => {
            let contents = async_io::read_to_string_sync(path)
                .with_context(|| read_schema_context(path.display()))?;
            parse_schema_file_contents(&contents, path)
        }
    }
}

/// Resolves a schema source through the cancellation-aware filesystem worker
/// used by timed workflow steps. Inline schemas remain an inexpensive clone;
/// file-backed schemas are read in bounded chunks and Unix special files are
/// opened non-blocking by [`async_io::read_file`].
pub(crate) async fn load_schema_value_cancellable(
    source: &SchemaSource,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value> {
    match source {
        SchemaSource::Inline(schema) => Ok(schema.clone()),
        SchemaSource::File(path) => {
            let contents =
                async_io::read_to_string_cancellable(path, cancellation, async_io::MAX_READ_BYTES)
                    .await
                    .with_context(|| read_schema_context(path.display()))?;
            parse_schema_file_contents(&contents, path)
        }
    }
}

/// Checks `value` against `schema` well enough to catch the common mistakes:
/// recursively, through `properties`/`items`, every value present is checked
/// against its sub-schema's `type` (including a JSON Schema array-of-types)
/// and `enum`, and every object against its sub-schema's `required`. This
/// isn't full JSON Schema validation: `format`, `pattern`, numeric bounds,
/// `additionalProperties`, `oneOf`/`anyOf`/`allOf`, and `$ref` are not
/// checked, and a field the schema doesn't mention is never rejected (a
/// schema written for a Structured Outputs `output_schema` — which requires
/// `additionalProperties: false` in strict mode — must stay reusable as an
/// `input_schema` without also rejecting extra input fields). `what` names
/// the value in error messages (e.g. `"input"`, `"output"`, `"inputs.lang"`).
pub(crate) fn validate_value(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    what: &str,
) -> Result<()> {
    validate_value_against_schema(schema, value, what)
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

/// The pure, read-independent part of loading a file-backed Structured
/// Outputs schema, shared by every caller of [`load_json_schema_cancellable`].
fn parse_json_schema_contents(contents: &str, path: &Path, name: &str) -> Result<ResponseFormat> {
    let schema = serde_json::from_str::<serde_json::Value>(contents)
        .with_context(|| parse_schema_context(path.display()))?;
    build_json_schema(schema, name)
}

/// Loads a file-backed Structured Outputs schema for `--json-schema`/a
/// workflow step's file-backed `output_schema`. Cancellation-aware so a
/// `--json-schema` read joins the same `tokio::try_join!` as the request's
/// other independent reads (see `app::prepare_chat_request`) instead of
/// blocking ahead of it on a dedicated call.
pub(crate) async fn load_json_schema_cancellable(
    path: &Path,
    name: &str,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<ResponseFormat> {
    let contents =
        async_io::read_to_string_cancellable(path, cancellation, async_io::MAX_READ_BYTES)
            .await
            .with_context(|| read_schema_context(path.display()))?;
    parse_json_schema_contents(&contents, path, name)
}

/// Checks a Structured Outputs schema `name` (a step/agent's `schema_name`,
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
        SchemaKind::Deps => include_str!("../schemas/deps.json"),
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
