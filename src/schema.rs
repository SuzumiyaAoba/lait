use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use async_openai::types::chat::{ResponseFormat, ResponseFormatJsonSchema};

pub(crate) fn load_json_schema(path: &Path, name: &str) -> Result<ResponseFormat> {
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

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read JSON schema file '{}'", path.display()))?;
    let schema = serde_json::from_str::<serde_json::Value>(&contents)
        .with_context(|| format!("failed to parse JSON schema file '{}'", path.display()))?;

    Ok(ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: None,
            name: name.to_owned(),
            schema,
            strict: Some(true),
        },
    })
}
