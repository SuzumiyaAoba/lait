use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    future::Future,
    io,
    ops::Deref,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream::BoxStream};
use http::{HeaderName, HeaderValue, header::ACCEPT};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::{
    Peer, RoleClient, ServiceExt,
    model::{CallToolRequestParams, PaginatedRequestParams, Tool},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, Transport,
        async_rw::AsyncRwTransport,
        common::http_header::{
            EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
        },
        streamable_http_client::{
            AuthRequiredError, InsufficientScopeError, SseError, StreamableHttpClient,
            StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use sse_stream::{Sse, SseStream};
use tokio::{
    io::{AsyncRead, ReadBuf},
    process::{ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;

use crate::config;

/// Finite safety net for MCP handshakes, pagination requests, and tool calls.
/// Workflow nodes may still impose a shorter existing `timeout:`; this keeps
/// chat/agent calls and malformed or unresponsive MCP peers from waiting
/// forever when no workflow timeout exists.
const MCP_IO_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum bytes in one newline-delimited JSON-RPC message received from an
/// MCP stdio server.  `rmcp`'s default line buffer is unbounded, so enforce a
/// limit before it can materialize an attacker-controlled frame.
const MAX_STDIO_JSON_RPC_FRAME_BYTES: usize = 16 * 1024 * 1024;

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

type CancellationReceiver = tokio::sync::watch::Receiver<bool>;

/// The outer workflow timeout may race an MCP future.  Keep cancellation as
/// an explicit outcome so the caller can evict the exact cached connection
/// before returning; simply dropping `RunningService::call_tool` leaves the
/// server-side request in flight and makes a retry capable of duplicating a
/// side effect.
enum CancellationResult<T> {
    Completed(T),
    Cancelled,
}

async fn await_cancellation<F, T>(
    future: F,
    cancellation: Option<CancellationReceiver>,
) -> CancellationResult<T>
where
    F: Future<Output = T>,
{
    let Some(mut cancellation) = cancellation else {
        return CancellationResult::Completed(future.await);
    };

    let mut future = Box::pin(future);
    loop {
        if *cancellation.borrow() {
            return CancellationResult::Cancelled;
        }
        tokio::select! {
            biased;
            result = &mut future => {
                // Prefer cancellation even when the operation and the
                // sender become ready in the same turn.  A timeout handler
                // may already have started a cleanup/retry sequence by the
                // time this branch is polled; returning the result would
                // retain a connection whose request was part of that timed
                // out attempt.
                if *cancellation.borrow() {
                    return CancellationResult::Cancelled;
                }
                return CancellationResult::Completed(result);
            },
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    return CancellationResult::Cancelled;
                }
            }
        }
    }
}

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

/// One server name's connection cell.  The cancellation token is shared by
/// every caller waiting for the same first connection, so cancellation of any
/// one initializer cancels that shared attempt instead of leaving a child
/// process alive after the waiting task has gone away.
struct ConnectionCell {
    value: tokio::sync::OnceCell<Arc<McpConnection>>,
    cancellation: CancellationToken,
}

impl ConnectionCell {
    fn new() -> Self {
        Self {
            value: tokio::sync::OnceCell::new(),
            cancellation: CancellationToken::new(),
        }
    }
}

/// Stdio transport with an explicit process-tree owner.  rmcp's public
/// `TokioChildProcess` intentionally keeps the child wrapper private, so it
/// cannot be awaited by a cache eviction after the service is dropped.  This
/// small equivalent keeps the wrapped `ChildWrapper` until `close()` and
/// invokes its group/job-aware `kill()` before reporting cleanup complete.
struct ManagedStdioTransport {
    transport: AsyncRwTransport<RoleClient, FrameLimitedReader<ChildStdout>, ChildStdin>,
    child: Option<Box<dyn ChildWrapper>>,
    cleanup: Arc<CleanupWait>,
}

/// An `AsyncRead` adapter that rejects a newline-delimited frame before the
/// unbounded `rmcp::transport::async_rw` line buffer can grow past the safety
/// limit.  Reading into a small scratch buffer also means a large underlying
/// read cannot bypass the accounting or force a large temporary allocation.
struct FrameLimitedReader<R> {
    inner: R,
    frame_bytes: usize,
}

impl<R> FrameLimitedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            frame_bytes: 0,
        }
    }
}

impl<R> AsyncRead for FrameLimitedReader<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let destination = buf.initialize_unfilled();
        if destination.is_empty() {
            return std::task::Poll::Ready(Ok(()));
        }

        // `BufReader` normally asks for a few KiB. Keep this bound independent
        // of the caller so a future implementation cannot request an enormous
        // scratch buffer from an untrusted stream.
        const READ_CHUNK_BYTES: usize = 16 * 1024;
        let read_len = destination.len().min(READ_CHUNK_BYTES);
        let mut scratch = [0u8; READ_CHUNK_BYTES];
        let mut read_buf = ReadBuf::new(&mut scratch[..read_len]);
        match Pin::new(&mut self.inner).poll_read(cx, &mut read_buf) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Err(error)) => std::task::Poll::Ready(Err(error)),
            std::task::Poll::Ready(Ok(())) => {
                let bytes = read_buf.filled();
                for &byte in bytes {
                    if byte == b'\n' {
                        self.frame_bytes = 0;
                    } else {
                        self.frame_bytes = match self.frame_bytes.checked_add(1) {
                            Some(frame_bytes) if frame_bytes <= MAX_STDIO_JSON_RPC_FRAME_BYTES => {
                                frame_bytes
                            }
                            _ => {
                                return std::task::Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    format!(
                                        "MCP stdio JSON-RPC frame exceeds {} bytes",
                                        MAX_STDIO_JSON_RPC_FRAME_BYTES
                                    ),
                                )));
                            }
                        };
                    }
                }
                buf.put_slice(bytes);
                std::task::Poll::Ready(Ok(()))
            }
        }
    }
}

impl ManagedStdioTransport {
    async fn spawn(mut command: CommandWrap) -> std::io::Result<(Self, Arc<CleanupWait>)> {
        let mut child = command.spawn()?;
        let stdout = match child.stdout().take() {
            Some(stdout) => stdout,
            None => {
                let _ = Box::into_pin(child.kill()).await;
                return Err(std::io::Error::other("MCP child stdout was not piped"));
            }
        };
        let stdin = match child.stdin().take() {
            Some(stdin) => stdin,
            None => {
                let _ = Box::into_pin(child.kill()).await;
                return Err(std::io::Error::other("MCP child stdin was not piped"));
            }
        };
        let cleanup = CleanupWait::new();
        let transport = Self {
            transport: AsyncRwTransport::new(FrameLimitedReader::new(stdout), stdin),
            child: Some(child),
            cleanup: Arc::clone(&cleanup),
        };
        Ok((transport, cleanup))
    }

    async fn close_child(&mut self) -> std::io::Result<()> {
        let result = match self.child.take() {
            Some(mut child) => Box::into_pin(child.kill()).await,
            None => Ok(()),
        };
        self.cleanup.finish();
        result.map(|_| ())
    }
}

impl Drop for ManagedStdioTransport {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            self.cleanup.finish();
            return;
        };
        let cleanup = Arc::clone(&self.cleanup);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = Box::into_pin(child.kill()).await;
                cleanup.finish();
            });
        } else {
            // This transport is normally dropped by rmcp on a Tokio runtime.
            // A synchronous fallback still sends the tree-wide termination
            // signal if a caller drops it outside that runtime.
            let _ = child.start_kill();
            cleanup.finish();
        }
    }
}

impl Transport<RoleClient> for ManagedStdioTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.transport.send(item)
    }

    fn receive(
        &mut self,
    ) -> impl Future<Output = Option<rmcp::service::RxJsonRpcMessage<RoleClient>>> + Send {
        self.transport.receive()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async move {
            // Close stdin first so a cooperative MCP server may exit, then
            // kill the entire owned tree and await its reaping.  Even if the
            // pipe close reports an error, process cleanup must still happen.
            let transport_result = self.transport.close().await;
            let child_result = self.close_child().await;
            transport_result.and(child_result)
        }
    }
}

fn owned_process_command(command: tokio::process::Command) -> CommandWrap {
    let mut command = CommandWrap::from(command);
    // `JobObject` starts Windows children suspended while it attaches the
    // process to the job. If a later wrapper hook fails, Tokio must still kill
    // that suspended process when the temporary child is dropped.
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);
    command
}

/// One server name's entry in `McpRegistry::connections`: a cell so
/// concurrent first-time callers for the same name await one shared connect
/// instead of each racing to spawn their own (see `McpRegistry::connection`).
type ConnectionCellRef = Arc<ConnectionCell>;

/// One server name's entry in `McpRegistry::tool_lists`: a cell so concurrent
/// first-time callers for the same name await one shared `tools/list` round
/// trip instead of each issuing their own (see `McpRegistry::server_tools`).
type ToolListCell = Arc<tokio::sync::OnceCell<Arc<Vec<Tool>>>>;

/// A connected (or lazily-connectable) set of MCP servers, built once per
/// `lait run`/`lait agent run`/chat invocation and shared across every
/// completion request it makes — including concurrent ones (`parallel`/
/// `for_each` branches), which is why connections are cached behind a
/// `tokio::sync::Mutex`.
pub(crate) struct McpRegistry<'a> {
    servers: &'a config::McpServerMap,
    connections: tokio::sync::Mutex<HashMap<String, ConnectionCellRef>>,
    /// Each server's `tools/list` result, cached for the registry's lifetime:
    /// a server's tool list doesn't change over the course of one `lait run`/
    /// `lait agent run`/chat invocation, so every `tools()` call after the
    /// first for a given server reuses this instead of re-issuing the round
    /// trip (which a `for_each`/`loop` node with `mcp:` set would otherwise
    /// do on every iteration).
    tool_lists: tokio::sync::Mutex<HashMap<String, ToolListCell>>,
}

/// The OpenAI-shaped tool definitions for one completion request, plus the
/// bookkeeping needed to route a model's tool call back to the right MCP
/// server. Built fresh by `McpRegistry::tools` for every request from the
/// (possibly cached) per-server tool lists, since which servers are in play
/// can differ request to request even though each server's own tool list
/// doesn't.
pub(crate) struct ToolSet {
    pub(crate) tools: Vec<ChatCompletionTools>,
    /// Qualified tool name (`<server>__<tool>`, see `qualify_tool_name`) to
    /// the `(server, original tool name)` it came from.
    index: HashMap<String, (String, String)>,
}

impl ToolSet {
    /// Whether `qualified_name` (as returned in `tools`) names a tool in this
    /// set, used by `app::RequestSettings::complete` to route a model's tool
    /// call between this set and `subagent::ToolSet` when both are in play.
    pub(crate) fn contains(&self, qualified_name: &str) -> bool {
        self.index.contains_key(qualified_name)
    }
}

impl<'a> McpRegistry<'a> {
    pub(crate) fn new(servers: &'a config::McpServerMap) -> Self {
        Self {
            servers,
            connections: tokio::sync::Mutex::new(HashMap::new()),
            tool_lists: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Shut down every connection owned by this registry and wait until each
    /// transport has finished its cleanup. This must be called at the end of a
    /// successful invocation as well as on an error: relying on `Drop` can
    /// only signal cancellation and cannot await process-tree reaping.
    #[allow(dead_code, reason = "the top-level run owner may call this during final cleanup")]
    pub(crate) async fn shutdown(&self) {
        let cells = {
            let mut connections = self.connections.lock().await;
            std::mem::take(&mut *connections)
                .into_values()
                .collect::<Vec<_>>()
        };
        self.tool_lists.lock().await.clear();

        let mut shutdowns = Vec::new();
        for cell in cells {
            cell.cancellation.cancel();
            if let Some(connection) = cell.value.get().cloned() {
                shutdowns.push(async move {
                    connection.shutdown().await;
                });
            }
        }
        futures_util::future::join_all(shutdowns).await;
    }

    /// Connects to (or reuses an existing connection to) every server in
    /// `names`, lists their tools (or reuses a previously cached list), and
    /// returns them qualified and converted to OpenAI's `tools:` shape.
    /// Servers are connected to and listed concurrently (each is an
    /// independent round trip), not one at a time. `names` with no
    /// `mcp_servers:` entry is an error naming `lait.config.yml`'s
    /// `mcp_servers:`, since that can only be caught here (workflow/agent-file
    /// parsing never sees the config file).
    pub(crate) async fn tools(
        &self,
        names: &[String],
        cancellation: Option<CancellationReceiver>,
    ) -> Result<ToolSet> {
        let per_server = futures_util::future::try_join_all(names.iter().map(|name| {
            let cancellation = cancellation.clone();
            async move {
                let server_tools = self.server_tools(name, cancellation).await?;
                Ok::<_, anyhow::Error>((name.clone(), server_tools))
            }
        }))
        .await?;

        let mut tools = Vec::new();
        let mut index = HashMap::new();
        for (name, server_tools) in per_server {
            let Some(total_tools) = tools.len().checked_add(server_tools.len()) else {
                bail!("MCP tool count overflowed while building the tool set");
            };
            if total_tools > MAX_TOTAL_TOOLS {
                bail!(
                    "MCP server tool list exceeds the cumulative limit of {MAX_TOTAL_TOOLS} tools"
                );
            }
            for tool in server_tools.iter() {
                let qualified = qualify_tool_name("MCP tool", &name, &tool.name)?;
                if let Some((existing_server, existing_tool)) = index.get(&qualified) {
                    bail!(
                        "MCP tool name collision: '{name}'.'{}' and '{existing_server}'.'{existing_tool}' both qualify to '{qualified}'",
                        tool.name
                    );
                }
                index.insert(qualified.clone(), (name.clone(), tool.name.to_string()));
                tools.push(ChatCompletionTools::Function(ChatCompletionTool {
                    function: FunctionObject {
                        name: qualified,
                        description: tool.description.as_deref().map(str::to_owned),
                        parameters: Some(serde_json::Value::Object((*tool.input_schema).clone())),
                        strict: None,
                    },
                }));
            }
        }
        Ok(ToolSet { tools, index })
    }

    /// Returns `name`'s tool list, listing it (following pagination) on first
    /// use and caching the result for the registry's lifetime — see
    /// `tool_lists`. Locking follows the same pattern as `connection`: the
    /// lock is only held to fetch-or-insert the `OnceCell`, never across the
    /// `tools/list` round trip itself, so independent servers list
    /// concurrently and concurrent callers racing on the same new server
    /// share one round trip.
    async fn server_tools(
        &self,
        name: &str,
        cancellation: Option<CancellationReceiver>,
    ) -> Result<Arc<Vec<Tool>>> {
        let cell = self
            .tool_lists
            .lock()
            .await
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
            .clone();
        let tool_list_cell = Arc::clone(&cell);

        let initializer_cancellation = cancellation.clone();
        // Do not race this initializer itself against cancellation.  The
        // nested connection/list request observes the same receiver and
        // performs synchronous (from the caller's point of view) connection
        // shutdown before returning its error.  Dropping `get_or_try_init`
        // here would otherwise abandon a stdio child while a retry starts.
        let result = cell
            .get_or_try_init(|| async {
                let connection = self
                    .connection(name, initializer_cancellation.clone())
                    .await?;
                let server_tools =
                    match list_all_tools(&connection, initializer_cancellation.clone()).await {
                        Ok(server_tools) => server_tools,
                        Err(error) => {
                            // A failed list request may have left an in-flight
                            // request in the transport (especially for stdio). Do
                            // not retain that service in either cache: cancel it and
                            // let the next attempt establish a fresh connection.
                            self.invalidate_connection_and_tool_list(
                                name,
                                &connection,
                                &tool_list_cell,
                            )
                            .await;
                            return Err(error).with_context(|| {
                                format!("failed to list tools for MCP server '{name}'")
                            });
                        }
                    };
                Ok::<_, anyhow::Error>(Arc::new(server_tools))
            })
            .await;

        // If cancellation happened before `connection` exposed a service,
        // there is no connection cleanup path to evict this initializer's
        // exact tool-list cell. Remove it so a later attempt can start with
        // a fresh cell instead of retaining the cancelled one.
        if result.is_err()
            && cancellation
                .as_ref()
                .is_some_and(|receiver| *receiver.borrow())
        {
            self.remove_tool_list_cell(name, &tool_list_cell).await;
        }

        result.map(Arc::clone)
    }

    /// Calls `qualified_name` (as returned in `tool_set`) with `arguments_json`
    /// (the raw string a model's tool call carries) and returns the tool's
    /// output rendered as plain text, suitable for a `tool`-role message.
    pub(crate) async fn call(
        &self,
        tool_set: &ToolSet,
        qualified_name: &str,
        arguments_json: &str,
        cancellation: Option<CancellationReceiver>,
    ) -> Result<String> {
        let (server_name, tool_name) = tool_set
            .index
            .get(qualified_name)
            .ok_or_else(|| anyhow!("model called unknown tool '{qualified_name}'"))?;
        let connection = self.connection(server_name, cancellation.clone()).await?;

        let arguments = if arguments_json.trim().is_empty() {
            None
        } else {
            let value: serde_json::Value =
                serde_json::from_str(arguments_json).with_context(|| {
                    format!("failed to parse arguments for tool call '{qualified_name}' as JSON")
                })?;
            match value {
                serde_json::Value::Object(object) => Some(object),
                serde_json::Value::Null => None,
                _ => bail!(
                    "arguments for tool call '{qualified_name}' must be a JSON object, got {value}"
                ),
            }
        };

        let params = CallToolRequestParams::new(tool_name.clone());
        let params = match arguments {
            Some(arguments) => params.with_arguments(arguments),
            None => params,
        };
        let result = match await_cancellation(
            tokio::time::timeout(MCP_IO_TIMEOUT, connection.call_tool(params)),
            cancellation,
        )
        .await
        {
            CancellationResult::Completed(Ok(Ok(result))) => result,
            CancellationResult::Completed(Ok(Err(error))) => {
                // A service-level error means the request did not produce a
                // usable result.  Evict the connection as transport/protocol
                // failures can leave it out of sync, and make sure the
                // caller receives the actual error rather than trying to
                // render the nested `Result` as a tool result.
                self.invalidate_connection(server_name, &connection).await;
                return Err(anyhow!(
                    "MCP server '{server_name}' failed while running tool '{tool_name}': {error}"
                ));
            }
            CancellationResult::Completed(Err(_)) => {
                // The outer `timeout` above reports an `Elapsed`, not an MCP
                // service error.  A timed-out request can still be executing
                // in a remote/stdio server, so close and evict this service
                // before any retry can reuse it.
                self.invalidate_connection(server_name, &connection).await;
                return Err(anyhow!(
                    "MCP server '{server_name}' timed out after {}s while running tool '{tool_name}'",
                    MCP_IO_TIMEOUT.as_secs()
                ));
            }
            CancellationResult::Cancelled => {
                // Dropping only the call future does not stop a stdio
                // server's in-flight work. Cancel and evict the exact
                // connection so a later call cannot reuse that service and
                // accidentally duplicate a side effect.
                self.invalidate_connection(server_name, &connection).await;
                return Err(anyhow!(
                    "MCP server '{server_name}' was cancelled while running tool '{tool_name}'"
                ));
            }
        };

        Ok(render_tool_result(result))
    }

    /// Returns the running connection for `name`, connecting lazily (and
    /// caching the result) on first use. The lock is only ever held to
    /// fetch-or-insert the `OnceCell` for `name`, never across the connect
    /// itself (spawning a child process or doing an HTTP handshake, both real
    /// wall-clock I/O) — so connections to independent servers (e.g. two
    /// `parallel` branches each first-using a different server) proceed
    /// concurrently. Concurrent callers racing on the *same* new server name
    /// share one `OnceCell` and thus one connect: `get_or_try_init` runs the
    /// connect for exactly one of them and the rest await its result, so no
    /// connection is ever established and then discarded.
    async fn connection(
        &self,
        name: &str,
        cancellation: Option<CancellationReceiver>,
    ) -> Result<Arc<McpConnection>> {
        if cancellation
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow())
        {
            return Err(anyhow!("MCP operation was cancelled"));
        }

        loop {
            let cell = self
                .connections
                .lock()
                .await
                .entry(name.to_owned())
                .or_insert_with(|| Arc::new(ConnectionCell::new()))
                .clone();
            let initializer_cancellation = cell.cancellation.clone();

            // OnceCell initialization is shared by all callers.  A monitor
            // turns a caller's cancellation into cancellation of that shared
            // attempt; the initializer then remains alive long enough for
            // `connect` to close/reap its transport before this future ends.
            let monitor = cancellation.clone().map(|mut receiver| {
                let initializer_cancellation = initializer_cancellation.clone();
                tokio::spawn(async move {
                    if *receiver.borrow() {
                        initializer_cancellation.cancel();
                        return;
                    }
                    while receiver.changed().await.is_ok() {
                        if *receiver.borrow() {
                            initializer_cancellation.cancel();
                            return;
                        }
                    }
                })
            });
            let result = cell
                .value
                .get_or_try_init(|| async {
                    let server = self.servers.get(name).ok_or_else(|| {
                        anyhow!(
                            "unknown MCP server '{name}'; define it under 'mcp_servers:' in {}",
                            config::CONFIG_FILE_NAME
                        )
                    })?;
                    let transport = server.resolve_transport(name)?;
                    Ok::<_, anyhow::Error>(Arc::new(
                        connect(name, transport, initializer_cancellation.clone()).await?,
                    ))
                })
                .await;
            if let Some(monitor) = monitor {
                monitor.abort();
            }

            match result {
                Ok(connection) => {
                    let connection = Arc::clone(connection);
                    let cancelled = initializer_cancellation.is_cancelled()
                        || cancellation
                            .as_ref()
                            .is_some_and(|receiver| *receiver.borrow());
                    if cancelled {
                        connection.shutdown().await;
                        self.remove_connection_cell(name, &cell).await;
                        return Err(anyhow!("MCP operation was cancelled"));
                    }
                    if connection.is_closing() {
                        connection.wait_closed().await;
                        self.remove_connection_cell(name, &cell).await;
                        continue;
                    }
                    return Ok(connection);
                }
                Err(error) => {
                    self.remove_connection_cell(name, &cell).await;
                    return Err(error);
                }
            }
        }
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
                return Err(anyhow!("MCP operation was cancelled"));
            }
            result.map_err(|error| anyhow!("failed to initialize MCP server '{name}': {error}"))
        }
        _ = tokio::time::sleep(MCP_IO_TIMEOUT) => {
            cancellation.cancel();
            let _ = serve.await;
            Err(anyhow!(
                "timed out after {}s while initializing MCP server '{name}'",
                MCP_IO_TIMEOUT.as_secs()
            ))
        }
        _ = cancellation.cancelled() => {
            let _ = serve.await;
            Err(anyhow!("MCP operation was cancelled"))
        }
    }
}

/// Errors raised by the reqwest adapter that enforces the response-body
/// budget before handing bytes to rmcp's JSON/SSE parsers.
#[derive(Debug)]
enum LimitedHttpClientError {
    Request(reqwest::Error),
    BodyTooLarge { limit: usize },
    Cancelled,
}

impl std::fmt::Display for LimitedHttpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(error) => write!(f, "HTTP request failed: {error}"),
            Self::BodyTooLarge { limit } => {
                write!(f, "HTTP response body exceeds {limit} bytes")
            }
            Self::Cancelled => f.write_str("HTTP request was cancelled"),
        }
    }
}

impl std::error::Error for LimitedHttpClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Request(error) => Some(error),
            Self::BodyTooLarge { .. } | Self::Cancelled => None,
        }
    }
}

impl From<reqwest::Error> for LimitedHttpClientError {
    fn from(error: reqwest::Error) -> Self {
        Self::Request(error)
    }
}

/// A reqwest-backed rmcp client with a finite budget for every HTTP response.
/// The stock rmcp adapter parses JSON responses with `Response::json()`, which
/// has no byte limit. Keeping this small adapter here lets us reject a large
/// `Content-Length` before allocation and count chunked/SSE bodies as they
/// arrive.
#[derive(Clone)]
struct LimitedHttpClient {
    client: reqwest::Client,
    max_body_bytes: usize,
    cancellation: CancellationToken,
}

impl LimitedHttpClient {
    fn new(
        client: reqwest::Client,
        max_body_bytes: usize,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            client,
            max_body_bytes,
            cancellation,
        }
    }

    /// Reqwest does not know about the lifecycle of the rmcp worker.  A
    /// transport cancellation must therefore abort an in-flight request here
    /// rather than waiting for reqwest's (deliberately long) request timeout.
    /// Otherwise `McpConnection::wait_closed` cannot complete before a retry
    /// starts and a timed-out workflow can stall for minutes.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, StreamableHttpError<LimitedHttpClientError>> {
        let cancellation = self.cancellation.clone();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(StreamableHttpError::Client(
                LimitedHttpClientError::Cancelled,
            )),
            response = request.send() => response
                .map_err(LimitedHttpClientError::Request)
                .map_err(StreamableHttpError::Client),
        }
    }

    fn check_content_length(
        &self,
        response: &reqwest::Response,
    ) -> Result<(), StreamableHttpError<LimitedHttpClientError>> {
        if response
            .content_length()
            .is_some_and(|length| length > self.max_body_bytes as u64)
        {
            return Err(StreamableHttpError::Client(
                LimitedHttpClientError::BodyTooLarge {
                    limit: self.max_body_bytes,
                },
            ));
        }
        Ok(())
    }

    fn apply_custom_headers(
        &self,
        mut request: reqwest::RequestBuilder,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<reqwest::RequestBuilder, StreamableHttpError<LimitedHttpClientError>> {
        for (name, value) in custom_headers {
            // These headers are owned by the transport. The protocol-version
            // header is the one intentional exception: rmcp injects it into
            // the map after initialization and expects it to pass through.
            let reserved = [
                "accept",
                HEADER_SESSION_ID,
                HEADER_LAST_EVENT_ID,
                "mcp-protocol-version",
            ];
            if reserved
                .iter()
                .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved))
                && !name.as_str().eq_ignore_ascii_case("mcp-protocol-version")
            {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            request = request.header(name, value);
        }
        Ok(request)
    }

    fn limited_stream(
        response: reqwest::Response,
        max_body_bytes: usize,
        cancellation: CancellationToken,
    ) -> impl Stream<Item = Result<Bytes, LimitedHttpClientError>> + Send + 'static {
        let stream = response.bytes_stream();
        futures_util::stream::unfold(
            (stream, 0usize, false, cancellation),
            move |(mut stream, total, failed, cancellation)| async move {
                if failed {
                    return None;
                }
                if cancellation.is_cancelled() {
                    return Some((
                        Err(LimitedHttpClientError::Cancelled),
                        (stream, total, true, cancellation),
                    ));
                }
                let cancellation_wait = cancellation.clone();
                let next = tokio::select! {
                    biased;
                    _ = cancellation_wait.cancelled() => {
                        return Some((
                            Err(LimitedHttpClientError::Cancelled),
                            (stream, total, true, cancellation),
                        ));
                    }
                    chunk = stream.next() => chunk,
                };
                match next {
                    None => None,
                    Some(Err(error)) => Some((
                        Err(LimitedHttpClientError::Request(error)),
                        (stream, total, true, cancellation),
                    )),
                    Some(Ok(chunk)) => {
                        let Some(next_total) = total.checked_add(chunk.len()) else {
                            return Some((
                                Err(LimitedHttpClientError::BodyTooLarge {
                                    limit: max_body_bytes,
                                }),
                                (stream, total, true, cancellation),
                            ));
                        };
                        if next_total > max_body_bytes {
                            Some((
                                Err(LimitedHttpClientError::BodyTooLarge {
                                    limit: max_body_bytes,
                                }),
                                (stream, total, true, cancellation),
                            ))
                        } else {
                            Some((Ok(chunk), (stream, next_total, false, cancellation)))
                        }
                    }
                }
            },
        )
    }

    async fn read_body(
        &self,
        response: reqwest::Response,
    ) -> Result<Vec<u8>, StreamableHttpError<LimitedHttpClientError>> {
        self.check_content_length(&response)?;
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::Cancelled,
                ));
            }
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk
                .map_err(LimitedHttpClientError::Request)
                .map_err(StreamableHttpError::Client)?;
            let Some(next_len) = body.len().checked_add(chunk.len()) else {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::BodyTooLarge {
                        limit: self.max_body_bytes,
                    },
                ));
            };
            if next_len > self.max_body_bytes {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::BodyTooLarge {
                        limit: self.max_body_bytes,
                    },
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn drain_body(
        &self,
        response: reqwest::Response,
    ) -> Result<(), StreamableHttpError<LimitedHttpClientError>> {
        self.check_content_length(&response)?;
        let mut total = 0usize;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::Cancelled,
                ));
            }
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk
                .map_err(LimitedHttpClientError::Request)
                .map_err(StreamableHttpError::Client)?;
            let Some(next_total) = total.checked_add(chunk.len()) else {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::BodyTooLarge {
                        limit: self.max_body_bytes,
                    },
                ));
            };
            if next_total > self.max_body_bytes {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::BodyTooLarge {
                        limit: self.max_body_bytes,
                    },
                ));
            }
            total = next_total;
        }
        Ok(())
    }

    fn as_sse_stream(
        &self,
        response: reqwest::Response,
    ) -> BoxStream<'static, Result<Sse, SseError>> {
        SseStream::from_bytes_stream(Self::limited_stream(
            response,
            self.max_body_bytes,
            self.cancellation.clone(),
        ))
        .boxed()
    }
}

impl StreamableHttpClient for LimitedHttpClient {
    type Error = LimitedHttpClientError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            MAX_HTTP_RESPONSE_BODY_BYTES,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        _max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut request = self
            .client
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(auth_header) = auth_header {
            request = request.bearer_auth(auth_header);
        }
        request = self.apply_custom_headers(request, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = self.send(request.json(&message)).await?;
        self.check_content_length(&response)?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            && let Some(header) = response.headers().get(reqwest::header::WWW_AUTHENTICATE)
        {
            let header = header
                .to_str()
                .map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::from(
                        "invalid www-authenticate header value",
                    ))
                })?
                .to_owned();
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                header,
            )));
        }
        if status == reqwest::StatusCode::FORBIDDEN
            && let Some(header) = response.headers().get(reqwest::header::WWW_AUTHENTICATE)
        {
            let header = header
                .to_str()
                .map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::from(
                        "invalid www-authenticate header value",
                    ))
                })?
                .to_owned();
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(header, None),
            ));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        let content_length = response.content_length();
        let response_session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            self.drain_body(response).await?;
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        if status.is_success()
            && content_length == Some(0)
            && matches!(
                message,
                rmcp::model::ClientJsonRpcMessage::Notification(_)
                    | rmcp::model::ClientJsonRpcMessage::Response(_)
                    | rmcp::model::ClientJsonRpcMessage::Error(_)
            )
        {
            self.drain_body(response).await?;
            return Ok(StreamableHttpPostResponse::Accepted);
        }

        if !status.is_success() {
            let body = self.read_body(response).await?;
            if content_type
                .as_deref()
                .is_some_and(|value| value.starts_with(JSON_MIME_TYPE))
                && let Some(message) = parse_json_rpc_error(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(
                    message,
                    response_session_id,
                ));
            }
            let body = String::from_utf8_lossy(&body);
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {body}"),
            )));
        }

        match content_type.as_deref() {
            Some(value) if value.starts_with(EVENT_STREAM_MIME_TYPE) => Ok(
                StreamableHttpPostResponse::Sse(self.as_sse_stream(response), response_session_id),
            ),
            Some(value) if value.starts_with(JSON_MIME_TYPE) => {
                let body = self.read_body(response).await?;
                match serde_json::from_slice::<rmcp::model::ServerJsonRpcMessage>(&body) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(
                        message,
                        response_session_id,
                    )),
                    Err(_error) => Ok(StreamableHttpPostResponse::Accepted),
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let mut request = self.client.delete(uri.as_ref());
        if let Some(auth_header) = auth_header {
            request = request.bearer_auth(auth_header);
        }
        request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        request = self.apply_custom_headers(request, custom_headers)?;
        let response = self.send(request).await?;
        self.check_content_length(&response)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        let response = response
            .error_for_status()
            .map_err(LimitedHttpClientError::Request)
            .map_err(StreamableHttpError::Client)?;
        self.drain_body(response).await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            MAX_HTTP_RESPONSE_BODY_BYTES,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        _max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let mut request = self
            .client
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        if let Some(auth_header) = auth_header {
            request = request.bearer_auth(auth_header);
        }
        request = self.apply_custom_headers(request, custom_headers)?;
        let response = self.send(request).await?;
        self.check_content_length(&response)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        let response = response
            .error_for_status()
            .map_err(LimitedHttpClientError::Request)
            .map_err(StreamableHttpError::Client)?;
        match response.headers().get(reqwest::header::CONTENT_TYPE) {
            Some(value)
                if value
                    .as_bytes()
                    .starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                    || value.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) =>
            {
                Ok(self.as_sse_stream(response))
            }
            Some(value) => Err(StreamableHttpError::UnexpectedContentType(Some(
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            ))),
            None => Err(StreamableHttpError::UnexpectedContentType(None)),
        }
    }
}

fn parse_json_rpc_error(body: &[u8]) -> Option<rmcp::model::ServerJsonRpcMessage> {
    serde_json::from_slice::<rmcp::model::ServerJsonRpcMessage>(body)
        .ok()
        .filter(|message| matches!(message, rmcp::model::ServerJsonRpcMessage::Error(_)))
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
        return Err(anyhow!("MCP operation was cancelled"));
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
            let mut header_map = HashMap::with_capacity(headers.len());
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
            let transport_config = StreamableHttpClientTransportConfig::with_uri(url)
                .custom_headers(header_map)
                .max_sse_event_size(MAX_HTTP_RESPONSE_BODY_BYTES);
            let http_client = reqwest::Client::builder()
                .timeout(MCP_IO_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .with_context(|| format!("failed to configure MCP server '{name}' HTTP client"))?;
            let transport = StreamableHttpClientTransport::with_client(
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

impl<'a> McpRegistry<'a> {
    /// Cancels `connection`, waits for the rmcp service and its transport to
    /// finish cleanup, and only then removes it from the cache. Pointer
    /// identity matters: another task may already have installed a fresh
    /// connection while this timeout handler was being scheduled.
    async fn invalidate_connection(&self, name: &str, connection: &Arc<McpConnection>) {
        connection.begin_shutdown();
        connection.wait_closed().await;
        let mut connections = self.connections.lock().await;
        let should_remove = connections
            .get(name)
            .and_then(|cell| cell.value.get())
            .is_some_and(|current| Arc::ptr_eq(current, connection));
        if should_remove {
            connections.remove(name);
        }
    }

    /// Like [`Self::invalidate_connection`], also evicts the exact
    /// `tools/list` cell that failed. A cached list can be retained after a
    /// `tools/call` timeout (its definitions remain valid for a new
    /// connection), but a list initializer that failed must not be reused.
    async fn invalidate_connection_and_tool_list(
        &self,
        name: &str,
        connection: &Arc<McpConnection>,
        tool_list_cell: &ToolListCell,
    ) {
        connection.begin_shutdown();
        connection.wait_closed().await;
        let mut connections = self.connections.lock().await;
        let should_remove_connection = connections
            .get(name)
            .and_then(|cell| cell.value.get())
            .is_some_and(|current| Arc::ptr_eq(current, connection));
        if should_remove_connection {
            connections.remove(name);
        }
        drop(connections);

        let mut tool_lists = self.tool_lists.lock().await;
        let should_remove_tool_list = tool_lists
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, tool_list_cell));
        if should_remove_tool_list {
            tool_lists.remove(name);
        }
    }

    /// Removes a connection cell only when it is still the exact cell that a
    /// caller initialized. This prevents a late error from deleting a fresh
    /// retry that another task has already installed.
    async fn remove_connection_cell(&self, name: &str, cell: &ConnectionCellRef) {
        let mut connections = self.connections.lock().await;
        let should_remove = connections
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, cell));
        if should_remove {
            connections.remove(name);
        }
    }

    /// Removes one exact tools/list initializer from the cache.  This is used
    /// when cancellation wins before an initializer has exposed a connected
    /// service that [`invalidate_connection_and_tool_list`] could cancel.
    async fn remove_tool_list_cell(&self, name: &str, tool_list_cell: &ToolListCell) {
        let mut tool_lists = self.tool_lists.lock().await;
        let should_remove = tool_lists
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, tool_list_cell));
        if should_remove {
            tool_lists.remove(name);
        }
    }
}

/// Lists every tool a server exposes, following `next_cursor` pagination
/// until the server reports none left.
async fn list_all_tools(
    connection: &McpConnection,
    cancellation: Option<CancellationReceiver>,
) -> Result<Vec<Tool>> {
    let mut tools = Vec::new();
    let mut metadata_bytes = 0usize;
    let mut cursor = None;
    let mut seen_cursors = HashSet::new();
    let mut pages = 0usize;
    loop {
        if pages >= MAX_TOOL_LIST_PAGES {
            bail!("MCP server returned more than {MAX_TOOL_LIST_PAGES} pages from 'tools/list'");
        }
        pages += 1;
        let params = cursor
            .take()
            .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let result = match await_cancellation(
            tokio::time::timeout(MCP_IO_TIMEOUT, connection.list_tools(params)),
            cancellation.clone(),
        )
        .await
        {
            CancellationResult::Completed(Ok(result)) => result,
            CancellationResult::Completed(Err(error)) => {
                return Err(anyhow!(
                    "MCP server timed out after {}s while listing tools (page {pages}): {error}",
                    MCP_IO_TIMEOUT.as_secs()
                ));
            }
            CancellationResult::Cancelled => {
                return Err(anyhow!("MCP operation was cancelled while listing tools"));
            }
        };
        let result = result.map_err(|error| anyhow!("{error}"))?;
        if result.tools.len() > MAX_TOOLS_PER_PAGE {
            bail!(
                "MCP server returned {} tools in one 'tools/list' page; the maximum is {MAX_TOOLS_PER_PAGE}",
                result.tools.len()
            );
        }
        let Some(total_tools) = tools.len().checked_add(result.tools.len()) else {
            bail!("MCP tool count overflowed while listing tools");
        };
        if total_tools > MAX_TOTAL_TOOLS {
            bail!(
                "MCP server returned more than {MAX_TOTAL_TOOLS} tools across 'tools/list' pages"
            );
        }
        for tool in &result.tools {
            metadata_bytes = tool_metadata_bytes(tool, metadata_bytes)?;
        }
        tools.extend(result.tools);
        match result.next_cursor {
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    bail!("MCP server repeated a 'tools/list' pagination cursor '{next}'");
                }
                cursor = Some(next);
            }
            None => break,
        }
    }
    Ok(tools)
}

/// A writer used to measure serialized JSON without allocating a second copy
/// of a potentially large schema. It fails as soon as the caller's remaining
/// metadata budget is exhausted.
struct ByteCounter {
    bytes: usize,
    limit: usize,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.bytes.checked_add(bytes.len()) else {
            return Err(io::Error::other("MCP tool metadata byte count overflowed"));
        };
        if next > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "MCP tool descriptions and schemas exceed the {}-byte metadata limit",
                    self.limit
                ),
            ));
        }
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_json_bytes<T: serde::Serialize>(value: &T, limit: usize) -> Result<usize> {
    let mut counter = ByteCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("MCP tool schema exceeds the {limit}-byte metadata limit"))?;
    Ok(counter.bytes)
}

fn tool_metadata_bytes(tool: &Tool, used: usize) -> Result<usize> {
    let mut total = used;
    if let Some(description) = &tool.description {
        total = total
            .checked_add(description.len())
            .ok_or_else(|| anyhow!("MCP tool metadata byte count overflowed"))?;
        if total > MAX_TOOL_METADATA_BYTES {
            bail!(
                "MCP tool descriptions and schemas exceed the cumulative limit of {MAX_TOOL_METADATA_BYTES} bytes"
            );
        }
    }

    for schema in [Some(&tool.input_schema), tool.output_schema.as_ref()]
        .into_iter()
        .flatten()
    {
        let remaining = MAX_TOOL_METADATA_BYTES
            .checked_sub(total)
            .ok_or_else(|| anyhow!("MCP tool metadata byte count overflowed"))?;
        let schema_bytes = serialized_json_bytes(schema.as_ref(), remaining)?;
        total = total
            .checked_add(schema_bytes)
            .ok_or_else(|| anyhow!("MCP tool metadata byte count overflowed"))?;
    }
    Ok(total)
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
    parts.join("\n\n")
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
