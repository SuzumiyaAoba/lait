//! `lait trace export`: sends a `--trace-file`-written JSONL log to an
//! OpenTelemetry collector over OTLP/HTTP with the JSON encoding
//! (`POST <url>`, `Content-Type: application/json`), one span per
//! [`TraceEvent`]. Hand-built rather than via the `opentelemetry` crates:
//! a finished trace file only needs a one-shot serialization of already
//! recorded events, not a live SDK/exporter pipeline.
//!
//! The events are flat (see this module's parent doc comment), so every span
//! is a root span of one shared trace. Ids are derived from the file's
//! contents rather than random, so exporting the same file twice produces
//! the same trace/span ids instead of a duplicate trace.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::TraceEvent;
use crate::{cli::TraceExportArgs, config, report};

/// OTLP's `SPAN_KIND_CLIENT`: a model or tool call is an outgoing request.
const SPAN_KIND_CLIENT: u8 = 3;
/// OTLP's `SPAN_KIND_INTERNAL`, for lait's own bookkeeping (`compact`).
const SPAN_KIND_INTERNAL: u8 = 1;

pub(crate) async fn export_otlp(
    args: TraceExportArgs,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let contents = std::fs::read(&args.file)
        .with_context(|| format!("failed to read trace file '{}'", args.file.display()))?;
    let events = super::read_jsonl(&args.file)?;
    if events.is_empty() {
        report::note(format_args!(
            "'{}' has no events; nothing to export",
            args.file.display()
        ));
        return Ok(());
    }
    let body = to_otlp_json(&events, &args.service_name, &contents);

    let mut request = crate::llm::http_client()
        .post(&args.otlp)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body).context("failed to encode OTLP spans")?);
    for header in &args.headers {
        let (name, value) = header
            .split_once('=')
            .ok_or_else(|| anyhow!("--header '{header}' must be NAME=VALUE"))?;
        let value =
            config::expand_env_placeholders(value).with_context(|| format!("--header '{name}'"))?;
        request = request.header(name.trim(), value);
    }
    let response = tokio::select! {
        response = request.send() => response
            .with_context(|| format!("failed to send spans to '{}'", args.otlp))?,
        () = cancel.cancelled() => bail!(crate::error::cancelled("trace export was cancelled")),
    };
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!(
            "OTLP endpoint '{}' answered {status}: {}",
            args.otlp,
            text.trim()
        );
    }
    report::note(format_args!(
        "exported {} span{} to '{}'",
        events.len(),
        if events.len() == 1 { "" } else { "s" },
        args.otlp
    ));
    Ok(())
}

/// The OTLP `ExportTraceServiceRequest` JSON for `events`. `seed` (the trace
/// file's raw bytes) determines the trace/span ids; see this module's doc
/// comment.
fn to_otlp_json(events: &[TraceEvent], service_name: &str, seed: &[u8]) -> Value {
    let trace_id = hex_digest(&[seed], 16);
    let spans: Vec<Value> = events
        .iter()
        .map(|event| {
            let mut attributes = vec![
                key_value("gen_ai.operation.name", &Value::from(event.operation.as_str())),
                key_value("lait.label", &Value::from(event.label.as_str())),
                key_value("lait.seq", &Value::from(event.seq)),
            ];
            attributes.extend(
                event
                    .attributes
                    .iter()
                    .map(|(key, value)| key_value(key, value)),
            );
            json!({
                "traceId": trace_id,
                "spanId": hex_digest(&[trace_id.as_bytes(), &event.seq.to_be_bytes()], 8),
                "name": format!("{} {}", event.operation, event.label),
                "kind": if event.operation == "compact" { SPAN_KIND_INTERNAL } else { SPAN_KIND_CLIENT },
                "startTimeUnixNano": unix_nanos(event.start),
                "endTimeUnixNano": unix_nanos(event.end),
                "attributes": attributes,
            })
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [key_value("service.name", &Value::from(service_name))],
            },
            "scopeSpans": [{
                "scope": {"name": "lait", "version": env!("CARGO_PKG_VERSION")},
                "spans": spans,
            }],
        }],
    })
}

/// An OTLP `KeyValue`. OTLP/JSON encodes 64-bit integers as strings; an
/// array/object attribute (which OTLP could model as `arrayValue`/
/// `kvlistValue`) is sent as its JSON text, which every backend can display.
fn key_value(key: &str, value: &Value) -> Value {
    let any_value = match value {
        Value::String(text) => json!({"stringValue": text}),
        Value::Bool(flag) => json!({"boolValue": flag}),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => json!({"intValue": integer.to_string()}),
            None => json!({"doubleValue": number.as_f64()}),
        },
        Value::Null => json!({"stringValue": ""}),
        other => json!({"stringValue": other.to_string()}),
    };
    let mut entry = Map::new();
    entry.insert("key".to_owned(), Value::from(key));
    entry.insert("value".to_owned(), any_value);
    Value::Object(entry)
}

fn unix_nanos(time: chrono::DateTime<chrono::Utc>) -> String {
    time.timestamp_nanos_opt().unwrap_or_default().to_string()
}

/// The first `bytes` bytes of the SHA-256 of `parts`, as lowercase hex.
fn hex_digest(parts: &[&[u8]], bytes: usize) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize()[..bytes]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{key_value, to_otlp_json};
    use crate::trace::{TraceCollector, attrs};
    use serde_json::{Value, json};

    #[test]
    fn events_become_spans_of_one_trace_with_typed_attributes() {
        let collector = TraceCollector::default();
        let start = chrono::Utc::now();
        collector.record(
            "chat",
            "summarize",
            start,
            start + chrono::Duration::milliseconds(5),
            attrs([
                ("gen_ai.request.model", Value::from("m")),
                ("gen_ai.usage.input_tokens", Value::from(12)),
            ]),
        );
        collector.record("compact", "summarize", start, start, serde_json::Map::new());
        let body = to_otlp_json(&collector.events(), "svc", b"seed");

        let spans = body["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["traceId"], spans[1]["traceId"]);
        assert_eq!(spans[0]["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(spans[0]["spanId"].as_str().unwrap().len(), 16);
        assert_ne!(spans[0]["spanId"], spans[1]["spanId"]);
        assert_eq!(spans[0]["name"], "chat summarize");
        assert_eq!(spans[0]["kind"], 3);
        assert_eq!(spans[1]["kind"], 1);
        let attributes = spans[0]["attributes"].as_array().unwrap();
        assert!(
            attributes.contains(
                &json!({"key": "gen_ai.usage.input_tokens", "value": {"intValue": "12"}})
            )
        );
        assert!(
            attributes
                .contains(&json!({"key": "gen_ai.request.model", "value": {"stringValue": "m"}}))
        );
        assert_eq!(
            body["resourceSpans"][0]["resource"]["attributes"][0],
            json!({"key": "service.name", "value": {"stringValue": "svc"}})
        );
    }

    #[test]
    fn the_same_input_produces_the_same_ids() {
        let collector = TraceCollector::default();
        let start = chrono::Utc::now();
        collector.record("chat", "a", start, start, serde_json::Map::new());
        let events = collector.events();
        assert_eq!(
            to_otlp_json(&events, "lait", b"x"),
            to_otlp_json(&events, "lait", b"x")
        );
    }

    #[test]
    fn non_integer_and_structured_values_are_encoded_losslessly_enough() {
        assert_eq!(
            key_value("k", &json!(1.5))["value"],
            json!({"doubleValue": 1.5})
        );
        assert_eq!(
            key_value("k", &json!(true))["value"],
            json!({"boolValue": true})
        );
        assert_eq!(
            key_value("k", &json!(["a"]))["value"],
            json!({"stringValue": "[\"a\"]"})
        );
    }
}
