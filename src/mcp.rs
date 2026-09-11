//! MCP (Model Context Protocol) client: connects to `mcp_servers:` entries,
//! lists and calls their tools, and enforces the resource limits a
//! third-party server must not be trusted to respect on its own.
//!
//! Three largely independent concerns share the `MAX_*` byte/depth
//! constants below and the same `McpRegistry` entry point, so each has its
//! own submodule: [`registry`] (connection lifecycle, per-server tool-list
//! caching, tool-name qualification — re-exported here as `McpRegistry`/
//! `ToolSet`), [`stdio`] (spawns the server as a child process, wraps its
//! stdout in a frame-size-limited reader), and [`http_client`] (a
//! `reqwest`-backed `StreamableHttpClient` with its own Content-Length and
//! SSE-event-size enforcement, since `rmcp` does not cap either — named
//! `http_client` rather than `http` to avoid shadowing the `http` crate this
//! file also depends on). What stays here is what both transports and the
//! registry share: the constants, the transport-agnostic `McpConnection`/
//! `CleanupWait` lifecycle types, `connect` (which builds whichever
//! transport a server's config names and hands it to `serve_with_timeout`),
//! and `qualify_tool_name`/`render_tool_result`.
//!
//! `qualify_tool_name`, at the bottom, is shared with `subagent` — MCP tools
//! and subagent tools both need the same `server__tool` naming scheme so the
//! two capability kinds cannot collide in one tool-call dispatch table.

mod http_client;
mod registry;
mod stdio;

pub(crate) use registry::{McpRegistry, ToolSet};

use std::{
    ops::Deref,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::{Peer, RoleClient, ServiceExt, service::RunningService};
use tokio_util::sync::CancellationToken;

use crate::config;
use http_client::LimitedHttpClient;
use stdio::{ManagedStdioTransport, owned_process_command};

/// Finite safety net for MCP handshakes, pagination requests, and tool calls.
/// Workflow nodes may still impose a shorter existing `timeout:`; this keeps
/// chat/agent calls and malformed or unresponsive MCP peers from waiting
/// forever when no workflow timeout exists.
const MCP_IO_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum bytes in one HTTP response body received from an MCP server.  The
/// limit applies to both content-length responses and chunked/SSE streams.
const MAX_HTTP_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Bounds for a `tools/list` response after it has been decoded. A page and
/// the complete paginated list are limited independently so a peer cannot
/// bypass one bound by splitting a response across many pages.
const MAX_TOOLS_PER_PAGE: usize = 1_024;
const MAX_TOTAL_TOOLS: usize = 8_192;

/// Descriptions and JSON schemas are copied into every OpenAI tool definition.
/// Count their serialized UTF-8 bytes before retaining a list, including both
/// input and optional output schemas.
const MAX_TOOL_METADATA_BYTES: usize = 16 * 1024 * 1024;

/// A healthy server should expose its tools in a small number of pages. This
/// bound also makes an untrusted/misbehaving remote server unable to grow the
/// client-side tool list without limit by returning an endless sequence of
/// distinct cursors.
const MAX_TOOL_LIST_PAGES: usize = 128;

/// The app-level "this run was cancelled" signal threaded down from
/// `app.rs`, distinct from this module's own [`CancellationToken`] uses
/// below (each of those represents one MCP I/O operation's own timeout, not
/// the run as a whole). Kept as its own alias — rather than writing
/// `CancellationToken` at each of these call sites — precisely so that
/// distinction stays visible at a glance.
type CancellationReceiver = CancellationToken;

/// Completion state for a cleanup operation that is performed by another
/// task.  `Notify` is paired with the atomic flag so a waiter cannot miss a
/// notification between checking the state and going to sleep.
struct CleanupWait {
    complete: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CleanupWait {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            complete: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn finish(&self) {
        self.complete.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            if self.complete.load(Ordering::Acquire) {
                return;
            }
            let notified = self.notify.notified();
            if self.complete.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// A running MCP connection.  The rmcp `RunningService` owns the transport
/// and its service-loop task, while callers use the cloned `Peer`.  Keeping
/// the service in a dedicated waiter task lets invalidation await rmcp's
/// `transport.close()` all the way through process-tree termination/reaping;
/// merely dropping an `Arc<RunningService>` cannot provide that guarantee.
struct McpConnection {
    peer: Peer<RoleClient>,
    cancellation: CancellationToken,
    cleanup: Arc<CleanupWait>,
    closing: AtomicBool,
}

impl McpConnection {
    fn from_running(
        running: RunningService<RoleClient, ()>,
        cancellation: CancellationToken,
    ) -> Self {
        let peer = running.peer().clone();
        let cleanup = CleanupWait::new();
        let waiter_cleanup = Arc::clone(&cleanup);
        tokio::spawn(async move {
            let _ = running.waiting().await;
            waiter_cleanup.finish();
        });
        Self {
            peer,
            cancellation,
            cleanup,
            closing: AtomicBool::new(false),
        }
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    fn begin_shutdown(&self) {
        self.closing.store(true, Ordering::Release);
        self.cancellation.cancel();
    }

    async fn wait_closed(&self) {
        self.cleanup.wait().await;
    }

    /// Cancel the rmcp service and wait until its transport has completed
    /// cleanup.  This is intentionally not cancellation-aware: once a
    /// connection is invalidated, allowing its caller's cancellation to
    /// interrupt this wait would reintroduce a stale retry race.
    async fn shutdown(&self) {
        self.begin_shutdown();
        self.wait_closed().await;
    }
}

impl Deref for McpConnection {
    type Target = Peer<RoleClient>;

    fn deref(&self) -> &Self::Target {
        &self.peer
    }
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        // The service waiter task owns the actual `RunningService`; there is
        // no synchronous way to await it from Drop.  Cancelling here still
        // guarantees that an unused registry eventually closes its transport.
        self.cancellation.cancel();
    }
}

/// Runs an rmcp handshake with a bounded timeout while retaining the future
/// until it observes cancellation.  Retaining it is important for stdio:
/// dropping the handshake future drops the transport, and only the transport
/// can perform the awaited process-tree cleanup.
async fn serve_with_timeout<T, E, A>(
    name: &str,
    transport: T,
    cancellation: CancellationToken,
) -> Result<RunningService<RoleClient, ()>>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let serve = ().serve_with_ct(transport, cancellation.clone());
    tokio::pin!(serve);
    tokio::select! {
        biased;
        result = &mut serve => {
            if cancellation.is_cancelled() {
                return Err(anyhow!(crate::error::Interrupted::cancelled("MCP operation was cancelled")));
            }
            result.map_err(|error| anyhow!("failed to initialize MCP server '{name}': {error}"))
        }
        _ = tokio::time::sleep(MCP_IO_TIMEOUT) => {
            cancellation.cancel();
            let _ = serve.await;
            Err(anyhow!(crate::error::Interrupted::timed_out(format!("timed out after {}s while initializing MCP server '{name}'",
                MCP_IO_TIMEOUT.as_secs()))))
        }
        _ = cancellation.cancelled() => {
            let _ = serve.await;
            Err(anyhow!(crate::error::Interrupted::cancelled("MCP operation was cancelled")))
        }
    }
}

/// Opens one MCP connection over the given transport, using the default
/// (do-nothing) `ClientHandler` — lait only ever calls tools, so it never
/// needs to answer server-initiated requests (sampling, roots, elicitation).
async fn connect(
    name: &str,
    transport: config::McpTransport,
    cancellation: CancellationToken,
) -> Result<McpConnection> {
    if cancellation.is_cancelled() {
        return Err(anyhow!(crate::error::Interrupted::cancelled(
            "MCP operation was cancelled"
        )));
    }
    match transport {
        config::McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let mut process_command = tokio::process::Command::new(&command);
            process_command.args(&args).envs(&env);
            // `tokio::process::Command` inherits stdio by default.  rmcp's
            // JSON-RPC transport needs dedicated pipes, however: without
            // these settings `Child::stdout`/`stdin` are `None`, so the
            // server either cannot be read or receives no requests.
            process_command.stdin(Stdio::piped()).stdout(Stdio::piped());
            if let Some(cwd) = &cwd {
                process_command.current_dir(cwd);
            }
            let (transport, cleanup) =
                ManagedStdioTransport::spawn(owned_process_command(process_command))
                    .await
                    .with_context(|| {
                        format!("failed to spawn MCP server '{name}' (command '{command}')")
                    })?;
            match serve_with_timeout(name, transport, cancellation.clone()).await {
                Ok(running) => Ok(McpConnection::from_running(running, cancellation)),
                Err(error) => {
                    // On initialization failure rmcp has dropped the
                    // transport.  Wait for its Drop-spawned tree kill before
                    // allowing this OnceCell attempt to be retried.
                    cleanup.wait().await;
                    Err(error)
                }
            }
        }
        config::McpTransport::Http { url, headers } => {
            let mut header_map = std::collections::HashMap::with_capacity(headers.len());
            for (key, value) in &headers {
                let header_name =
                    http::HeaderName::from_bytes(key.as_bytes()).with_context(|| {
                        format!("mcp_servers.{name} has an invalid header name '{key}'")
                    })?;
                let header_value = http::HeaderValue::from_str(value).with_context(|| {
                    format!("mcp_servers.{name} has an invalid header value for '{key}'")
                })?;
                header_map.insert(header_name, header_value);
            }
            let transport_config =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url)
                    .custom_headers(header_map)
                    .max_sse_event_size(MAX_HTTP_RESPONSE_BODY_BYTES);
            let http_client = reqwest::Client::builder()
                .timeout(MCP_IO_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .with_context(|| format!("failed to configure MCP server '{name}' HTTP client"))?;
            let transport = rmcp::transport::StreamableHttpClientTransport::with_client(
                LimitedHttpClient::new(
                    http_client,
                    MAX_HTTP_RESPONSE_BODY_BYTES,
                    cancellation.clone(),
                ),
                transport_config,
            );
            let running = serve_with_timeout(name, transport, cancellation.clone()).await?;
            Ok(McpConnection::from_running(running, cancellation))
        }
    }
}

/// OpenAI function names must match `^[a-zA-Z0-9_-]{1,64}$`. Qualifies
/// `name` with `prefix` (so two different tools with the same bare name
/// don't collide) by joining them with `__`, replacing any other character
/// with `_`, and rejecting (rather than truncating, which risks a silent
/// second collision) a result over 64 characters. `kind` names what's being
/// qualified for the error message (e.g. `"MCP tool"`). Shared by MCP tool
/// names (`prefix` is the server name — see `ToolSet::tools`) and subagent
/// tool names (`prefix` is the fixed string `"agent"` — see
/// `subagent::AgentRegistry::tools`), so both sources of dynamically-offered
/// tools sanitize/length-check their qualified names the same way.
pub(crate) fn qualify_tool_name(kind: &str, prefix: &str, name: &str) -> Result<String> {
    let raw = format!("{prefix}__{name}");
    let qualified: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if qualified.is_empty() || qualified.len() > 64 {
        bail!(
            "{kind} name '{qualified}' (from '{prefix}', '{name}') is empty or exceeds OpenAI's 64-character function name limit"
        );
    }
    Ok(qualified)
}

/// Renders a `tools/call` result as plain text for a `tool`-role message:
/// text content blocks joined as-is, any other block type (image/audio/
/// resource) JSON-serialized so nothing is silently dropped. Structured
/// content is included in a separate labeled section, and `is_error: true`
/// is likewise labeled so the model can distinguish a failed tool from a
/// successful result while still seeing every returned value.
/// Takes `result` by value (the caller never reuses it) so a text block's
/// content can be moved into the output instead of cloned — tool output can
/// be large (file contents, search results, ...).
fn render_tool_result(result: rmcp::model::CallToolResult) -> String {
    let is_error = result.is_error == Some(true);
    let mut parts = result
        .content
        .into_iter()
        .map(|block| match block {
            rmcp::model::ContentBlock::Text(text) => text.text,
            other => serde_json::to_string(&other)
                .expect("MCP content blocks should always be JSON-serializable"),
        })
        .collect::<Vec<_>>();
    if let Some(structured_content) = result.structured_content {
        let structured_content = serde_json::to_string(&structured_content)
            .expect("MCP structured content should always be JSON-serializable");
        parts.push(format!("structuredContent:\n{structured_content}"));
    }
    if is_error {
        parts.insert(0, "isError: true".to_owned());
    }
    // The common case (a single text content block, no structured content,
    // no error) has nothing to join — `parts.join(..)` would still allocate
    // a full copy of that one block just to hand back an equivalent
    // `String`. `pop` moves it out instead.
    match parts.len() {
        0 => String::new(),
        1 => parts.pop().expect("parts.len() == 1"),
        _ => parts.join("\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::{qualify_tool_name, render_tool_result};
    use rmcp::model::CallToolResult;
    use serde_json::json;

    #[test]
    fn qualifies_a_tool_name_with_its_server() {
        assert_eq!(
            qualify_tool_name("MCP tool", "filesystem", "read_file").unwrap(),
            "filesystem__read_file"
        );
    }

    #[test]
    fn sanitizes_invalid_characters() {
        assert_eq!(
            qualify_tool_name("MCP tool", "my server", "tool.name").unwrap(),
            "my_server__tool_name"
        );
    }

    #[test]
    fn rejects_a_name_over_64_characters() {
        let long_tool = "a".repeat(60);
        assert!(qualify_tool_name("MCP tool", "server", &long_tool).is_err());
    }

    #[test]
    fn preserves_structured_content_without_text_content() {
        let result: CallToolResult = serde_json::from_value(json!({
            "structuredContent": {"answer": 42},
            "isError": false,
        }))
        .expect("structured MCP result should deserialize");

        assert_eq!(
            render_tool_result(result),
            "structuredContent:\n{\"answer\":42}"
        );
    }

    #[test]
    fn preserves_structured_content_alongside_text_content() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [{"type": "text", "text": "plain result"}],
            "structuredContent": {"answer": 42},
        }))
        .expect("structured MCP result should deserialize");

        assert_eq!(
            render_tool_result(result),
            "plain result\n\nstructuredContent:\n{\"answer\":42}"
        );
    }

    #[test]
    fn preserves_the_error_flag_alongside_structured_content() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [{"type": "text", "text": "details"}],
            "structuredContent": {"code": "invalid"},
            "isError": true,
        }))
        .expect("error MCP result should deserialize");

        assert_eq!(
            render_tool_result(result),
            "isError: true\n\ndetails\n\nstructuredContent:\n{\"code\":\"invalid\"}"
        );
    }
}
