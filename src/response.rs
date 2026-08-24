use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    message: ChatCompletionResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponseMessage {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    content: &'a str,
    reasoning: Option<&'a str>,
}

pub(crate) fn render_response(
    response: &ChatCompletionResponse,
    as_json: bool,
    show_reasoning: bool,
) -> Result<String> {
    let content = response_content(response).map_err(anyhow::Error::msg)?;
    let reasoning = response_reasoning(response);

    if as_json {
        Ok(serde_json::to_string(&JsonOutput { content, reasoning })?)
    } else {
        Ok(format_response(content, reasoning, show_reasoning))
    }
}

fn response_content(response: &ChatCompletionResponse) -> std::result::Result<&str, &'static str> {
    let choice = response
        .choices
        .first()
        .ok_or("API response contained no choices")?;
    choice
        .message
        .content
        .as_deref()
        .filter(|content| !content.is_empty())
        .ok_or("API response contained no content in its first choice")
}

fn response_reasoning(response: &ChatCompletionResponse) -> Option<&str> {
    response.choices.first().and_then(|choice| {
        choice
            .message
            .reasoning
            .as_deref()
            .filter(|reasoning| !reasoning.trim().is_empty())
            .or_else(|| {
                choice
                    .message
                    .reasoning_content
                    .as_deref()
                    .filter(|reasoning| !reasoning.trim().is_empty())
            })
    })
}

fn format_response(content: &str, reasoning: Option<&str>, show_reasoning: bool) -> String {
    match (show_reasoning, reasoning) {
        (true, Some(reasoning)) if !reasoning.trim().is_empty() => {
            format!("Reasoning:\n{reasoning}\n\n{content}")
        }
        _ => content.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatCompletionResponse, format_response, response_content, response_reasoning};

    #[test]
    fn rejects_empty_choices_or_content() {
        let no_choices = serde_json::from_value::<ChatCompletionResponse>(serde_json::json!({
            "choices": []
        }))
        .expect("response fixture should deserialize");
        assert_eq!(
            response_content(&no_choices),
            Err("API response contained no choices")
        );

        let no_content = serde_json::from_value::<ChatCompletionResponse>(serde_json::json!({
            "choices": [{
                "message": {"content": null}
            }]
        }))
        .expect("response fixture should deserialize");
        assert_eq!(
            response_content(&no_content),
            Err("API response contained no content in its first choice")
        );
    }

    #[test]
    fn formats_reasoning_when_requested() {
        assert_eq!(
            format_response("answer", Some("step one"), true),
            "Reasoning:\nstep one\n\nanswer"
        );
        assert_eq!(format_response("answer", Some("  \n"), true), "answer");
        assert_eq!(format_response("answer", Some("step one"), false), "answer");
        assert_eq!(format_response("answer", None, true), "answer");
    }

    #[test]
    fn reads_current_and_legacy_reasoning_fields() {
        let current = serde_json::from_value::<ChatCompletionResponse>(serde_json::json!({
            "choices": [{
                "message": {
                    "content": "answer",
                    "reasoning": "current reasoning",
                    "reasoning_content": "legacy reasoning"
                }
            }]
        }))
        .expect("response fixture should deserialize");
        assert_eq!(response_reasoning(&current), Some("current reasoning"));

        let legacy_fallback = serde_json::from_value::<ChatCompletionResponse>(serde_json::json!({
            "choices": [{
                "message": {
                    "content": "answer",
                    "reasoning": "  ",
                    "reasoning_content": "legacy reasoning"
                }
            }]
        }))
        .expect("response fixture should deserialize");
        assert_eq!(
            response_reasoning(&legacy_fallback),
            Some("legacy reasoning")
        );
    }
}
