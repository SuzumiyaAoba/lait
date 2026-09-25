//! A client for Jev-compatible decision APIs — TypeSafe AI's "System One"
//! `POST /v1/systemone` and the open reimplementations that mirror it. Unlike
//! every other model call lait makes, Jev produces no text: it answers a
//! fixed set of typed questions (`noul` yes/no probability, `choice` over
//! named options, `score` over ordered levels) about one piece of `state`,
//! each with calibrated probabilities. Used by a workflow's `decide:` step
//! (`workflow::exec`) and `lait decide` (`app::decide`); the endpoint comes
//! from `lait.config.yml`'s `jev:` block (`config::JevConfig`).
//!
//! Questions are validated up front ([`Questions::parse`]) against the
//! request rules of the community conformance spec (jevcompat SPEC 0.1),
//! so a malformed question fails at workflow parse/lint time rather than as
//! a 422 mid-run. Responses are checked only as far as lait relies on them
//! (one answer per question, of the requested `type`, carrying that type's
//! value field) and otherwise passed through verbatim: `confidence`'s
//! formula differs between servers, so lait never recomputes it.
//!
//! Not (yet) integrated: `--show-usage`/`lait history`/`--trace-file`
//! (a Jev response's `usage` is returned by `lait decide --full` only),
//! the response disk cache, and `--record`/`--replay`.

use std::ops::RangeInclusive;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::{
    config::{ConfigFile, Endpoint},
    engine::AppServices,
    llm,
};

/// TypeSafe's hosted API. Like every other lait `base_url`, it ends in the
/// version segment; requests go to `{base_url}/systemone`.
pub(crate) const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai/v1";

/// The alias every Jev-compatible server must accept (jevcompat
/// `request.model-alias`).
pub(crate) const DEFAULT_MODEL: &str = "jev-latest";

/// The model a request names: an explicit override (a `decide:` step's
/// `model:`, `lait decide --model`) > `jev.model` > [`DEFAULT_MODEL`].
pub(crate) fn resolve_model(model_override: Option<&str>, file_config: &ConfigFile) -> String {
    model_override
        .or(file_config.jev.model.as_deref())
        .unwrap_or(DEFAULT_MODEL)
        .to_owned()
}

/// Options per `choice` question (jevcompat `choice.options`).
const CHOICE_OPTIONS: RangeInclusive<usize> = 2..=255;

/// Levels per `score` question (jevcompat `score.levels`).
const SCORE_LEVELS: RangeInclusive<usize> = 2..=10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuestionType {
    Noul,
    Choice,
    Score,
}

impl QuestionType {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "noul" => Some(Self::Noul),
            "choice" => Some(Self::Choice),
            "score" => Some(Self::Score),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Noul => "noul",
            Self::Choice => "choice",
            Self::Score => "score",
        }
    }

    /// The field that carries this type's answer value.
    fn value_field(self) -> &'static str {
        match self {
            Self::Noul => "noul",
            Self::Choice => "choice",
            Self::Score => "score",
        }
    }
}

/// A validated `questions` object, kept in declaration order (which is
/// also the key order of the answers lait returns).
#[derive(Debug, Clone)]
pub(crate) struct Questions {
    body: Map<String, Value>,
    types: Vec<(String, QuestionType)>,
}

impl Questions {
    /// Validates a `questions` value: a non-empty object mapping question
    /// ids to `{type, instructions?, criteria?}`.
    pub(crate) fn parse(value: Value) -> Result<Self> {
        let Value::Object(body) = value else {
            bail!("questions must be a mapping of question ids to question definitions");
        };
        if body.is_empty() {
            bail!("questions must define at least one question");
        }
        let mut types = Vec::with_capacity(body.len());
        for (id, question) in &body {
            if id.is_empty() {
                bail!("question ids must be non-empty");
            }
            let question_type =
                check_question(question).with_context(|| format!("question '{id}'"))?;
            types.push((id.clone(), question_type));
        }
        Ok(Self { body, types })
    }

    /// Parses YAML (or JSON, a YAML subset) questions. YAML reads the
    /// unquoted `noul` criteria keys `true:`/`false:` as booleans, which a
    /// JSON object cannot hold, so every scalar key is stringified first.
    pub(crate) fn from_yaml(value: serde_yaml::Value) -> Result<Self> {
        Self::parse(yaml_to_json(value)?)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, QuestionType)> {
        self.types
            .iter()
            .map(|(id, question_type)| (id.as_str(), *question_type))
    }

    pub(crate) fn len(&self) -> usize {
        self.types.len()
    }
}

fn check_question(question: &Value) -> Result<QuestionType> {
    let Value::Object(fields) = question else {
        bail!("must be a mapping with 'type' (and optionally 'instructions'/'criteria')");
    };
    if let Some(key) = fields
        .keys()
        .find(|key| !matches!(key.as_str(), "type" | "instructions" | "criteria"))
    {
        bail!("unknown field '{key}'; questions accept: type, instructions, criteria");
    }
    let type_name = fields
        .get("type")
        .ok_or_else(|| anyhow!("'type' is required (noul, choice, or score)"))?
        .as_str()
        .ok_or_else(|| anyhow!("'type' must be a string"))?;
    let question_type = QuestionType::parse(type_name).ok_or_else(|| {
        anyhow!("unknown type '{type_name}'; expected one of: noul, choice, score")
    })?;
    if let Some(instructions) = fields.get("instructions")
        && !(instructions.is_null() || is_structured_text(instructions))
    {
        bail!("'instructions' must be a string, a mapping, or a list");
    }
    let criteria = fields.get("criteria").filter(|value| !value.is_null());
    match question_type {
        QuestionType::Noul => {
            if let Some(criteria) = criteria {
                let Value::Object(sides) = criteria else {
                    bail!("a noul question's 'criteria' must map 'true'/'false' to descriptions");
                };
                for (side, description) in sides {
                    if side != "true" && side != "false" {
                        bail!("a noul question's 'criteria' only accepts 'true' and 'false' keys");
                    }
                    check_description(description, &format!("criteria.{side}"))?;
                }
            }
        }
        QuestionType::Choice => {
            let Some(Value::Object(options)) = criteria else {
                bail!("a choice question needs 'criteria' mapping option names to descriptions");
            };
            if !CHOICE_OPTIONS.contains(&options.len()) {
                bail!(
                    "a choice question needs {} to {} options, found {}",
                    CHOICE_OPTIONS.start(),
                    CHOICE_OPTIONS.end(),
                    options.len()
                );
            }
            for (option, description) in options {
                if option.is_empty() {
                    bail!("choice option names must be non-empty");
                }
                check_description(description, &format!("criteria.{option}"))?;
            }
        }
        QuestionType::Score => {
            let Some(Value::Array(levels)) = criteria else {
                bail!("a score question needs 'criteria' as an ordered list of level descriptions");
            };
            if !SCORE_LEVELS.contains(&levels.len()) {
                bail!(
                    "a score question needs {} to {} levels, found {}",
                    SCORE_LEVELS.start(),
                    SCORE_LEVELS.end(),
                    levels.len()
                );
            }
            for (index, level) in levels.iter().enumerate() {
                if !is_structured_text(level) {
                    bail!("criteria[{index}] must be a string, a mapping, or a list");
                }
            }
        }
    }
    Ok(question_type)
}

/// A criterion description: structured text, or `null` for "the name says
/// it all" (jevcompat `choice.null-description`).
fn check_description(description: &Value, at: &str) -> Result<()> {
    if description.is_null() || is_structured_text(description) {
        Ok(())
    } else {
        bail!("'{at}' must be a string, a mapping, a list, or null")
    }
}

/// jevcompat `request.structured-text`: a string, object, or array.
fn is_structured_text(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_))
}

fn yaml_to_json(value: serde_yaml::Value) -> Result<Value> {
    Ok(match value {
        serde_yaml::Value::Mapping(mapping) => {
            let mut object = Map::with_capacity(mapping.len());
            for (key, value) in mapping {
                let key = match key {
                    serde_yaml::Value::String(key) => key,
                    serde_yaml::Value::Bool(key) => key.to_string(),
                    serde_yaml::Value::Number(key) => key.to_string(),
                    _ => bail!("mapping keys must be strings"),
                };
                object.insert(key, yaml_to_json(value)?);
            }
            Value::Object(object)
        }
        serde_yaml::Value::Sequence(items) => {
            Value::Array(items.into_iter().map(yaml_to_json).collect::<Result<_>>()?)
        }
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(tagged.value)?,
        scalar => serde_json::to_value(scalar).context("unsupported YAML value")?,
    })
}

/// Converts a workflow value into a `state`: strings, objects, and arrays
/// are sent as they are (jevcompat `request.state`); any other scalar is
/// sent as its text form.
pub(crate) fn state_from_value(value: Value) -> Value {
    match value {
        Value::String(_) | Value::Object(_) | Value::Array(_) => value,
        Value::Null => Value::String(String::new()),
        other => Value::String(other.to_string()),
    }
}

/// One decision request's parsed reply.
#[derive(Debug)]
pub(crate) struct Decision {
    /// The server's full response body, for `lait decide --full`.
    pub(crate) raw: Value,
    /// `answers`, keyed and ordered like the request's questions.
    pub(crate) answers: Map<String, Value>,
}

/// Sends one decision request to `{endpoint.base_url}/systemone`.
pub(crate) async fn decide(
    services: &AppServices,
    endpoint: &Endpoint,
    model: &str,
    questions: &Questions,
    state: Value,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<Decision> {
    let api_key = services
        .secret_resolver
        .resolve(&endpoint.api_key, cancellation.clone())
        .await?;
    let body = serde_json::json!({
        "model": model,
        "state": state,
        "questions": questions.body,
    });
    let body = serde_json::to_string(&body).context("failed to serialize the Jev request")?;
    tracing::trace!(request = %body, "sending Jev decision request");

    let url = format!("{}/systemone", endpoint.base_url);
    let mut request = llm::http_client()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = with_cancellation(request.send(), &cancellation)
        .await?
        .with_context(|| format!("failed to request {url}"))?;
    let status = response.status();
    let text = with_cancellation(response.text(), &cancellation)
        .await?
        .with_context(|| format!("failed to read the response from {url}"))?;
    tracing::trace!(response = %text, "received Jev decision response");
    if !status.is_success() {
        bail!("POST {url} failed with {status}: {}", error_message(&text));
    }
    let raw: Value = serde_json::from_str(&text)
        .with_context(|| format!("the response from {url} is not valid JSON"))?;
    let answers = check_answers(&raw, questions)
        .with_context(|| format!("unexpected response from {url}"))?;
    Ok(Decision { raw, answers })
}

async fn with_cancellation<T>(
    future: impl Future<Output = T>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            Err(crate::error::cancelled("Jev decision request was cancelled"))
        }
        result = future => Ok(result),
    }
}

/// The readable part of an error body: `detail.message` (jevcompat
/// `auth.missing`), the joined `detail[].msg` of a 422 validation error, a
/// string `detail`, or else the body itself.
fn error_message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let detail = parsed.as_ref().and_then(|value| value.get("detail"));
    let message = match detail {
        Some(Value::String(message)) => Some(message.clone()),
        Some(Value::Object(detail)) => detail
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Some(Value::Array(errors)) => {
            let messages: Vec<String> = errors
                .iter()
                .filter_map(|error| {
                    let message = error.get("msg")?.as_str()?;
                    Some(match error.get("loc") {
                        Some(location) => format!("{location}: {message}"),
                        None => message.to_owned(),
                    })
                })
                .collect();
            (!messages.is_empty()).then(|| messages.join("; "))
        }
        _ => None,
    };
    message.unwrap_or_else(|| body.trim().to_owned())
}

/// Checks the parts of a response lait relies on and returns `answers` in
/// the request's question order.
fn check_answers(raw: &Value, questions: &Questions) -> Result<Map<String, Value>> {
    let answers = raw
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("missing an 'answers' object"))?;
    if let Some(extra) = answers.keys().find(|id| {
        !questions
            .iter()
            .any(|(question, _)| question == id.as_str())
    }) {
        bail!("answer '{extra}' does not match any question");
    }
    let mut ordered = Map::with_capacity(questions.len());
    for (id, question_type) in questions.iter() {
        let answer = answers
            .get(id)
            .ok_or_else(|| anyhow!("no answer for question '{id}'"))?;
        let answer_type = answer.get("type").and_then(Value::as_str);
        if answer_type != Some(question_type.name()) {
            bail!(
                "the answer for '{id}' has type {}, expected '{}'",
                answer_type.map_or_else(|| "(none)".to_owned(), |name| format!("'{name}'")),
                question_type.name()
            );
        }
        let field = question_type.value_field();
        let value = answer.get(field);
        let valid = match question_type {
            QuestionType::Noul | QuestionType::Score => value.is_some_and(Value::is_number),
            QuestionType::Choice => value.is_some_and(Value::is_string),
        };
        if !valid {
            bail!("the answer for '{id}' is missing a valid '{field}' field");
        }
        ordered.insert(id.to_owned(), answer.clone());
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn questions(value: Value) -> Questions {
        Questions::parse(value).expect("questions should be valid")
    }

    #[test]
    fn accepts_the_three_question_types() {
        let parsed = questions(json!({
            "urgent": {"type": "noul", "instructions": "Is it urgent?", "criteria": {"true": "yes", "false": "no"}},
            "team": {"type": "choice", "criteria": {"billing": "money", "sales": null}},
            "anger": {"type": "score", "instructions": {"focus": "tone"}, "criteria": ["Calm", "Frustrated", "Very angry"]},
            "bare": {"type": "noul"},
        }));
        let types: Vec<_> = parsed.iter().collect();
        assert_eq!(
            types,
            vec![
                ("urgent", QuestionType::Noul),
                ("team", QuestionType::Choice),
                ("anger", QuestionType::Score),
                ("bare", QuestionType::Noul),
            ]
        );
    }

    #[test]
    fn rejects_malformed_questions() {
        for (value, expected) in [
            (json!({}), "at least one question"),
            (json!([]), "mapping"),
            (json!({"q": {"criteria": {}}}), "'type' is required"),
            (json!({"q": {"type": "rank"}}), "unknown type 'rank'"),
            (
                json!({"q": {"type": "noul", "extra": 1}}),
                "unknown field 'extra'",
            ),
            (
                json!({"q": {"type": "noul", "instructions": 3}}),
                "'instructions'",
            ),
            (
                json!({"q": {"type": "noul", "criteria": {"maybe": "x"}}}),
                "'true' and 'false'",
            ),
            (
                json!({"q": {"type": "choice", "criteria": {"only": null}}}),
                "2 to 255 options",
            ),
            (
                json!({"q": {"type": "choice", "criteria": ["a", "b"]}}),
                "mapping option names",
            ),
            (
                json!({"q": {"type": "choice", "criteria": {"a": 1, "b": null}}}),
                "criteria.a",
            ),
            (
                json!({"q": {"type": "score", "criteria": ["one"]}}),
                "2 to 10 levels",
            ),
            (
                json!({"q": {"type": "score", "criteria": {"0": "low", "1": "high"}}}),
                "ordered list",
            ),
            (json!({"": {"type": "noul"}}), "non-empty"),
        ] {
            let error = Questions::parse(value.clone()).expect_err(&value.to_string());
            assert!(
                format!("{error:#}").contains(expected),
                "{value}: {error:#} should mention {expected:?}"
            );
        }
        let many: Map<String, Value> = (0..256)
            .map(|index| (format!("o{index}"), Value::Null))
            .collect();
        assert!(Questions::parse(json!({"q": {"type": "choice", "criteria": many}})).is_err());
    }

    #[test]
    fn yaml_boolean_keys_become_noul_sides() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "urgent:\n  type: noul\n  criteria:\n    true: needs action today\n    false: can wait\n",
        )
        .unwrap();
        let parsed = Questions::from_yaml(yaml).unwrap();
        assert_eq!(
            parsed.body["urgent"]["criteria"],
            json!({"true": "needs action today", "false": "can wait"})
        );
    }

    #[test]
    fn state_keeps_structured_values_and_stringifies_scalars() {
        assert_eq!(state_from_value(json!("text")), json!("text"));
        assert_eq!(state_from_value(json!({"a": 1})), json!({"a": 1}));
        assert_eq!(state_from_value(json!([1])), json!([1]));
        assert_eq!(state_from_value(json!(42)), json!("42"));
        assert_eq!(state_from_value(json!(true)), json!("true"));
        assert_eq!(state_from_value(Value::Null), json!(""));
    }

    #[test]
    fn answers_are_checked_and_reordered_to_the_question_order() {
        let parsed = questions(json!({
            "urgent": {"type": "noul"},
            "team": {"type": "choice", "criteria": {"billing": null, "sales": null}},
        }));
        let raw = json!({
            "model": "jev-1.13.0",
            "answers": {
                "team": {"type": "choice", "choice": "billing", "probabilities": {"billing": 0.9, "sales": 0.1}, "confidence": 0.8},
                "urgent": {"type": "noul", "noul": 0.95},
            },
            "usage": {"input_tokens": 10, "output_tokens": 2},
        });
        let answers = check_answers(&raw, &parsed).unwrap();
        assert_eq!(answers.keys().collect::<Vec<_>>(), vec!["urgent", "team"]);
        assert_eq!(answers["urgent"]["noul"], json!(0.95));
    }

    #[test]
    fn rejects_answers_that_do_not_match_the_questions() {
        let parsed = questions(json!({"urgent": {"type": "noul"}}));
        for (raw, expected) in [
            (json!({}), "'answers'"),
            (json!({"answers": {}}), "no answer for question 'urgent'"),
            (
                json!({"answers": {"urgent": {"type": "noul", "noul": 0.5}, "x": {}}}),
                "'x' does not match",
            ),
            (
                json!({"answers": {"urgent": {"type": "score", "score": 1.0}}}),
                "expected 'noul'",
            ),
            (
                json!({"answers": {"urgent": {"type": "noul"}}}),
                "valid 'noul' field",
            ),
        ] {
            let error = check_answers(&raw, &parsed).expect_err(&raw.to_string());
            assert!(
                error.to_string().contains(expected),
                "{raw}: {error} should mention {expected:?}"
            );
        }
    }

    #[test]
    fn error_messages_prefer_the_detail_field() {
        assert_eq!(
            error_message(
                r#"{"detail":{"error_type":"authentication_error","message":"bad key"}}"#
            ),
            "bad key"
        );
        assert_eq!(
            error_message(r#"{"detail":[{"loc":["body","state"],"msg":"field required"}]}"#),
            r#"["body","state"]: field required"#
        );
        assert_eq!(error_message(r#"{"detail":"nope"}"#), "nope");
        assert_eq!(error_message("plain failure\n"), "plain failure");
    }
}
