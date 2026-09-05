//! Presentation of completed model responses.

use anyhow::Result;
use serde::Serialize;

use super::{ChatCompletionResponse, Usage, response_content, response_reasoning};

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    content: &'a str,
    reasoning: Option<&'a str>,
    /// Always present in `--json` output, including as `null` when the server
    /// did not report usage.
    usage: Option<Usage>,
}

/// Renders a completed response for the CLI's text or JSON output mode.
pub(crate) fn render_response(
    response: &ChatCompletionResponse,
    as_json: bool,
    show_reasoning: bool,
) -> Result<String> {
    let content = response_content(response).map_err(anyhow::Error::msg)?;
    let reasoning = response_reasoning(response);

    if as_json {
        Ok(serde_json::to_string(&JsonOutput {
            content,
            reasoning,
            usage: response.usage,
        })?)
    } else {
        Ok(format_response(content, reasoning, show_reasoning))
    }
}

/// Renders already-extracted text using the same shape as a completed
/// response's `--json` representation.
pub(crate) fn render_text_json(content: &str, usage: Option<Usage>) -> Result<String> {
    Ok(serde_json::to_string(&JsonOutput {
        content,
        reasoning: None,
        usage,
    })?)
}

/// Formats plain text and optional reasoning for terminal output.
pub(crate) fn format_response(
    content: &str,
    reasoning: Option<&str>,
    show_reasoning: bool,
) -> String {
    match (show_reasoning, reasoning) {
        (true, Some(reasoning)) if !reasoning.trim().is_empty() => {
            format!("Reasoning:\n{reasoning}\n\n{content}")
        }
        _ => content.to_owned(),
    }
}
