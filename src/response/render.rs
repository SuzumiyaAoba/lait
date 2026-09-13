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

/// The presentation flags [`render_response`] needs: whether to render as
/// `--json` and whether to include the model's reasoning text ahead of the
/// final content (`--show-reasoning`). Bundled into one named struct because
/// the two bare bools used to sit directly adjacent at every call site
/// (`render_response(&response, false, false)`), letting one silently stand
/// in for the other if swapped — the compiler cannot catch two `bool`s in
/// the wrong order the way it would a type mismatch.
pub(crate) struct RenderOptions {
    pub(crate) as_json: bool,
    pub(crate) show_reasoning: bool,
}

/// Renders a completed response for the CLI's text or JSON output mode.
pub(crate) fn render_response(
    response: &ChatCompletionResponse,
    options: RenderOptions,
) -> Result<String> {
    let content = response_content(response).map_err(anyhow::Error::msg)?;
    let reasoning = response_reasoning(response);

    if options.as_json {
        Ok(serde_json::to_string(&JsonOutput {
            content,
            reasoning,
            usage: response.usage,
        })?)
    } else {
        Ok(format_response(content, reasoning, options.show_reasoning))
    }
}

/// Renders a completed response as plain text with no reasoning preamble —
/// `render_response(response, RenderOptions { as_json: false, show_reasoning:
/// false })`, spelled out identically at three call sites
/// (`app::run_prompt`, `engine::agent::call_agent`,
/// `workflow::exec::nodes::execute_prompt`) before this helper existed. Not
/// for `app::run_chat`'s two call sites, which vary `as_json`/
/// `show_reasoning` with `--json`/`--show-reasoning` and so need
/// `render_response` directly.
pub(crate) fn render_plain(response: &ChatCompletionResponse) -> Result<String> {
    render_response(
        response,
        RenderOptions {
            as_json: false,
            show_reasoning: false,
        },
    )
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
