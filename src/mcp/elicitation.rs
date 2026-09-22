//! `LaitClientHandler`: the `ClientHandler` every MCP connection this
//! process opens actually uses (replacing the do-nothing `()` handler every
//! connection used before this module existed). Its only real behavior is
//! answering an `elicitation/create` request — a server asking, mid
//! `tools/call`, for information from the user (SEP-2322's Multi
//! Round-Trip Requests). `sampling`/`roots` are left at rmcp's own default
//! (decline/empty) implementations: both were deprecated by SEP-2577 ahead
//! of the 2026-07-28 protocol revision, so lait does not add new client
//! support for them — see `docs/usage/ja/mcp.md`'s elicitation section.
//!
//! Answering is opt-in per server (`mcp_servers.<name>.allow_elicitation`):
//! a server that can interactively prompt the user for information mid-call
//! is a trust escalation, the same way an unrestricted `allowed_tools` is.
//! With it off (the default) every request is declined immediately, with no
//! prompt at all — matching rmcp's own out-of-the-box behavior.

use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::{
    ClientHandler, RoleClient,
    model::{
        ClientCapabilities, ElicitRequestParams, ElicitResult, ElicitationAction,
        ElicitationSchema, Implementation, InitializeRequestParams, PrimitiveSchemaDefinition,
    },
    service::RequestContext,
};
use tokio_util::sync::CancellationToken;

use crate::{async_io, process, report};

fn decline() -> ElicitResult {
    ElicitResult::new(ElicitationAction::Decline)
}

fn cancel() -> ElicitResult {
    ElicitResult::new(ElicitationAction::Cancel)
}

/// The `ClientHandler` for one MCP connection (`mcp::connect`'s only
/// caller). One instance per connection, built with that server's own
/// `allow_elicitation`/name; `gate` is shared across every connection this
/// process opens (owned by `McpRegistry`) so two servers eliciting at the
/// same moment can't interleave their stdin prompts.
pub(super) struct LaitClientHandler {
    server_name: String,
    allow_elicitation: bool,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl LaitClientHandler {
    pub(super) fn new(
        server_name: String,
        allow_elicitation: bool,
        gate: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            server_name,
            allow_elicitation,
            gate,
        }
    }

    async fn prompt_form(
        &self,
        message: &str,
        schema: &ElicitationSchema,
        cancellation: CancellationToken,
    ) -> ElicitResult {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            tracing::debug!(
                server = %self.server_name,
                "declining elicitation: stdin is not an interactive terminal",
            );
            return decline();
        }
        let Some(_lease) = acquire_gate(&self.gate, &cancellation).await else {
            return decline();
        };

        eprintln!("mcp_servers.{}: {message}", self.server_name);
        for (name, property) in &schema.properties {
            let required = schema
                .required
                .as_ref()
                .is_some_and(|required| required.iter().any(|field| field == name));
            eprintln!(
                "  - {name}{}: {}",
                if required { " (required)" } else { "" },
                describe_property(property)
            );
        }
        eprint!("answer as a JSON object matching the fields above, or 'decline'/'cancel': ");

        let answer = match read_line_cancellable(cancellation).await {
            Ok(answer) => answer,
            Err(error) => {
                tracing::debug!(server = %self.server_name, error = %error, "elicitation read failed");
                return decline();
            }
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "decline" => decline(),
            "cancel" => cancel(),
            trimmed => match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(value @ serde_json::Value::Object(_)) => {
                    ElicitResult::new(ElicitationAction::Accept).with_content(value)
                }
                _ => {
                    report::note(format_args!(
                        "mcp_servers.{}: elicitation answer was not a JSON object; declining",
                        self.server_name
                    ));
                    decline()
                }
            },
        }
    }
}

/// A short, human-readable rendering of one elicitation field's expected
/// shape — the type name plus its `description`/`title`/`enum`, when
/// present. Serializes through JSON rather than matching each
/// `PrimitiveSchemaDefinition` variant by hand: every variant already
/// carries `description`/`title` under the same field names, so this stays
/// correct if a future rmcp version adds another primitive kind.
fn describe_property(property: &PrimitiveSchemaDefinition) -> String {
    let value = serde_json::to_value(property).unwrap_or(serde_json::Value::Null);
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("value");
    let description = value
        .get("description")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("title").and_then(serde_json::Value::as_str));
    match description {
        Some(description) => format!("{kind} — {description}"),
        None => kind.to_owned(),
    }
}

/// Acquires `gate`, giving up (returning `None`) if `cancellation` fires
/// first — mirrors `engine::approval`'s own `acquire_approval_gate`.
async fn acquire_gate(
    gate: &Arc<tokio::sync::Mutex<()>>,
    cancellation: &CancellationToken,
) -> Option<tokio::sync::OwnedMutexGuard<()>> {
    let gate = Arc::clone(gate);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => None,
        lease = gate.lock_owned() => Some(lease),
    }
}

/// Reads one line from stdin on a cancellable worker — see
/// `workflow::ask::run_ask`'s matching doc comment: the blocking read
/// itself cannot be interrupted mid-syscall, but a cancelled caller still
/// gives up promptly rather than waiting for it.
async fn read_line_cancellable(cancellation: CancellationToken) -> Result<String> {
    use std::io::BufRead;
    async_io::run_blocking(
        move |_cancelled| {
            let stdin = std::io::stdin();
            let mut buffer = String::new();
            stdin
                .lock()
                .read_line(&mut buffer)
                .context("failed to read from stdin")?;
            Ok(process::strip_one_trailing_line_ending(buffer))
        },
        cancellation,
    )
    .await
}

impl ClientHandler for LaitClientHandler {
    /// Always declares the `elicitation` capability, even for a connection
    /// whose `allow_elicitation` is `false` — that flag only decides how
    /// `create_elicitation` *answers* a request (see its own doc comment),
    /// not whether the server may send one; declining is itself a normal,
    /// spec-compliant answer.
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::builder().enable_elicitation().build(),
            Implementation::new("lait", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        if !self.allow_elicitation {
            tracing::debug!(
                server = %self.server_name,
                "declining elicitation: 'allow_elicitation' is not set",
            );
            return Ok(decline());
        }
        Ok(match request {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                self.prompt_form(&message, &requested_schema, context.ct.clone())
                    .await
            }
            ElicitRequestParams::UrlElicitationParams { message, url, .. } => {
                report::note(format_args!(
                    "mcp_servers.{}: {message} (declining automatic follow-up — open this URL \
                     yourself if you want to continue: {url})",
                    self.server_name
                ));
                decline()
            }
            // `ElicitRequestParams` is `#[non_exhaustive]`: a future rmcp
            // version may add another mode. Decline it the same way every
            // unsupported/disabled case here does, rather than failing the
            // whole request.
            _ => decline(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::describe_property;
    use rmcp::model::{PrimitiveSchemaDefinition, StringSchema};

    #[test]
    fn describe_property_includes_the_type_and_description() {
        let schema =
            PrimitiveSchemaDefinition::String(StringSchema::new().description("a city name"));
        assert_eq!(describe_property(&schema), "string — a city name");
    }

    #[test]
    fn describe_property_falls_back_to_just_the_type_with_no_description() {
        let schema = PrimitiveSchemaDefinition::String(StringSchema::default());
        assert_eq!(describe_property(&schema), "string");
    }
}
