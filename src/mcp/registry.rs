//! The registry itself: connection lifecycle (lazy connect, shared
//! in-flight-connect cells, invalidation on failure/timeout/cancellation),
//! per-server tool-list caching, and the request-time policy checks
//! (`allowed_tools`, cumulative tool/metadata-byte limits) that apply
//! regardless of which transport (`mcp/stdio.rs`/`mcp/http_client.rs`) a server
//! uses. `McpRegistry`/`ToolSet` are re-exported at `crate::mcp` since every
//! other module reaches this type through that path.

use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use rmcp::model::{CallToolRequestParams, PaginatedRequestParams, Tool};

use crate::{
    async_io::{CancellationResult, await_cancellation},
    config,
};

use super::{
    CancellationReceiver, MAX_TOOL_LIST_PAGES, MAX_TOOL_METADATA_BYTES, MAX_TOOLS_PER_PAGE,
    MAX_TOTAL_TOOLS, MCP_IO_TIMEOUT, McpConnection, connect, qualify_tool_name, render_tool_result,
};

/// One server name's entry in `McpRegistry::connections`: a cell so
/// concurrent first-time callers for the same name await one shared connect
/// instead of each racing to spawn their own (see `McpRegistry::connection`).
type ConnectionCellRef = Arc<ConnectionCell>;

/// One server name's connection cell.  The cancellation token is shared by
/// every caller waiting for the same first connection, so cancellation of any
/// one initializer cancels that shared attempt instead of leaving a child
/// process alive after the waiting task has gone away.
struct ConnectionCell {
    value: tokio::sync::OnceCell<Arc<McpConnection>>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl ConnectionCell {
    fn new() -> Self {
        Self {
            value: tokio::sync::OnceCell::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }
}

/// One server name's entry in `McpRegistry::tool_lists`: a cell so concurrent
/// first-time callers for the same name await one shared `tools/list` round
/// trip instead of each issuing their own (see `McpRegistry::server_tools`).
type ToolListCell = Arc<tokio::sync::OnceCell<Arc<Vec<Tool>>>>;

/// A connected (or lazily-connectable) set of MCP servers, built once per
/// `lait run`/`lait agent run`/chat invocation and shared across every
/// completion request it makes — including concurrent ones (`parallel`/
/// `for_each` branches), which is why connections are cached behind a
/// `tokio::sync::Mutex`.
pub(crate) struct McpRegistry {
    servers: Arc<config::McpServerMap>,
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
    /// set, used by `engine::RequestSettings::complete` to route a model's tool
    /// call between this set and `subagent::ToolSet` when both are in play.
    pub(crate) fn contains(&self, qualified_name: &str) -> bool {
        self.index.contains_key(qualified_name)
    }
}

impl McpRegistry {
    pub(crate) fn new(servers: Arc<config::McpServerMap>) -> Self {
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
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
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
        self.check_tool_is_allowed(server_name, tool_name)?;
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
                return Err(anyhow!(crate::error::Interrupted::timed_out(format!(
                    "MCP server '{server_name}' timed out after {}s while running tool '{tool_name}'",
                    MCP_IO_TIMEOUT.as_secs()
                ))));
            }
            CancellationResult::Cancelled => {
                // Dropping only the call future does not stop a stdio
                // server's in-flight work. Cancel and evict the exact
                // connection so a later call cannot reuse that service and
                // accidentally duplicate a side effect.
                self.invalidate_connection(server_name, &connection).await;
                return Err(anyhow!(crate::error::Interrupted::cancelled(format!(
                    "MCP server '{server_name}' was cancelled while running tool '{tool_name}'"
                ))));
            }
        };

        Ok(render_tool_result(result))
    }

    /// Enforces `mcp_servers.<name>.allowed_tools`, if the server's config
    /// sets it: the field is absent by default (unrestricted, matching
    /// lait's behavior before this gate existed), but a present list —
    /// including an empty one, which denies every tool on the server —
    /// restricts which of the server's tools the model may call. Checked in
    /// `call` before opening a connection, so a disallowed call never
    /// reaches the server at all (unlike `tools()`, filtering the
    /// advertised list there would be bypassable by a model naming a tool
    /// it was never offered).
    fn check_tool_is_allowed(&self, server_name: &str, tool_name: &str) -> Result<()> {
        let Some(server) = self.servers.get(server_name) else {
            return Ok(());
        };
        let Some(allowed_tools) = &server.allowed_tools else {
            return Ok(());
        };
        if allowed_tools.iter().any(|allowed| allowed == tool_name) {
            return Ok(());
        }
        bail!(
            "MCP server '{server_name}' does not allow calling tool '{tool_name}' \
             (not listed in its 'allowed_tools' in {}); add it there if this call \
             should be permitted",
            config::CONFIG_FILE_NAME
        );
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
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(anyhow!(crate::error::Interrupted::cancelled(
                "MCP operation was cancelled"
            )));
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
            let monitor = cancellation.clone().map(|cancellation| {
                let initializer_cancellation = initializer_cancellation.clone();
                tokio::spawn(async move {
                    cancellation.cancelled().await;
                    initializer_cancellation.cancel();
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
                            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled);
                    if cancelled {
                        connection.shutdown().await;
                        self.remove_connection_cell(name, &cell).await;
                        return Err(anyhow!(crate::error::Interrupted::cancelled(
                            "MCP operation was cancelled"
                        )));
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
                return Err(anyhow!(crate::error::Interrupted::timed_out(format!(
                    "MCP server timed out after {}s while listing tools (page {pages}): {error}",
                    MCP_IO_TIMEOUT.as_secs()
                ))));
            }
            CancellationResult::Cancelled => {
                return Err(anyhow!(crate::error::Interrupted::cancelled(
                    "MCP operation was cancelled while listing tools"
                )));
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

#[cfg(test)]
mod tests {
    use super::{McpRegistry, serialized_json_bytes};
    use crate::config::McpServerConfig;
    use serde_json::json;
    use std::{collections::HashMap, sync::Arc};

    fn server_with_allowed_tools(allowed_tools: Option<Vec<String>>) -> McpRegistry {
        let mut servers = HashMap::new();
        servers.insert(
            "fs".to_owned(),
            McpServerConfig {
                command: Some("true".to_owned()),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
                url: None,
                headers: HashMap::new(),
                allowed_tools,
            },
        );
        McpRegistry::new(Arc::new(servers))
    }

    #[test]
    fn allows_any_tool_when_allowed_tools_is_unset() {
        let registry = server_with_allowed_tools(None);
        assert!(registry.check_tool_is_allowed("fs", "read_file").is_ok());
        assert!(registry.check_tool_is_allowed("fs", "write_file").is_ok());
    }

    #[test]
    fn allows_a_tool_named_in_the_allowlist() {
        let registry = server_with_allowed_tools(Some(vec!["read_file".to_owned()]));
        assert!(registry.check_tool_is_allowed("fs", "read_file").is_ok());
    }

    #[test]
    fn rejects_a_tool_not_named_in_the_allowlist() {
        let registry = server_with_allowed_tools(Some(vec!["read_file".to_owned()]));
        let error = registry
            .check_tool_is_allowed("fs", "write_file")
            .unwrap_err();
        assert!(error.to_string().contains("write_file"));
        assert!(error.to_string().contains("allowed_tools"));
    }

    #[test]
    fn an_empty_allowlist_denies_every_tool() {
        let registry = server_with_allowed_tools(Some(vec![]));
        assert!(registry.check_tool_is_allowed("fs", "read_file").is_err());
    }

    #[test]
    fn serialized_schema_size_is_checked_without_a_second_buffer() {
        let schema = json!({"type": "object", "description": "large"});
        let exact_size = serde_json::to_vec(&schema).unwrap().len();

        assert_eq!(
            serialized_json_bytes(&schema, exact_size).unwrap(),
            exact_size
        );
        let error = serialized_json_bytes(&schema, exact_size - 1)
            .expect_err("schema over the remaining metadata budget must fail");
        assert!(error.to_string().contains("metadata limit"));
    }
}
