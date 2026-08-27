use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, PaginatedRequestParams, Tool},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

use crate::config;

/// A connected (or lazily-connectable) set of MCP servers, built once per
/// `lait run`/`lait agent run`/chat invocation and shared across every
/// completion request it makes — including concurrent ones (`parallel`/
/// `for_each` branches), which is why connections are cached behind a
/// `tokio::sync::Mutex` rather than opened per-request.
pub(crate) struct McpRegistry {
    servers: config::McpServerMap,
    connections: tokio::sync::Mutex<HashMap<String, Arc<RunningService<RoleClient, ()>>>>,
}

/// The OpenAI-shaped tool definitions for one completion request, plus the
/// bookkeeping needed to route a model's tool call back to the right MCP
/// server. Built fresh by `McpRegistry::tools` for every request (server tool
/// lists can change between requests), never cached on the registry itself.
pub(crate) struct ToolSet {
    pub(crate) tools: Vec<ChatCompletionTools>,
    /// Qualified tool name (`<server>__<tool>`, see `qualify_tool_name`) to
    /// the `(server, original tool name)` it came from.
    index: HashMap<String, (String, String)>,
}

impl McpRegistry {
    pub(crate) fn new(servers: config::McpServerMap) -> Self {
        Self {
            servers,
            connections: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Connects to (or reuses an existing connection to) every server in
    /// `names`, lists their tools, and returns them qualified and converted
    /// to OpenAI's `tools:` shape. Servers are connected to and listed
    /// concurrently (each is an independent round trip), not one at a time.
    /// `names` with no `mcp_servers:` entry is an error naming
    /// `lait.config.yml`'s `mcp_servers:`, since that can only be caught here
    /// (workflow/agent-file parsing never sees the config file).
    pub(crate) async fn tools(&self, names: &[String]) -> Result<ToolSet> {
        let per_server = futures_util::future::try_join_all(names.iter().map(|name| async move {
            let connection = self.connection(name).await?;
            let server_tools = list_all_tools(&connection)
                .await
                .with_context(|| format!("failed to list tools for MCP server '{name}'"))?;
            Ok::<_, anyhow::Error>((name.clone(), server_tools))
        }))
        .await?;

        let mut tools = Vec::new();
        let mut index = HashMap::new();
        for (name, server_tools) in per_server {
            for tool in server_tools {
                let qualified = qualify_tool_name(&name, &tool.name)?;
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
                        description: tool.description.map(|description| description.into_owned()),
                        parameters: Some(serde_json::Value::Object((*tool.input_schema).clone())),
                        strict: None,
                    },
                }));
            }
        }
        Ok(ToolSet { tools, index })
    }

    /// Calls `qualified_name` (as returned in `tool_set`) with `arguments_json`
    /// (the raw string a model's tool call carries) and returns the tool's
    /// output rendered as plain text, suitable for a `tool`-role message.
    pub(crate) async fn call(
        &self,
        tool_set: &ToolSet,
        qualified_name: &str,
        arguments_json: &str,
    ) -> Result<String> {
        let (server_name, tool_name) = tool_set
            .index
            .get(qualified_name)
            .ok_or_else(|| anyhow!("model called unknown tool '{qualified_name}'"))?;
        let connection = self.connection(server_name).await?;

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
        let result = connection.call_tool(params).await.with_context(|| {
            format!("MCP server '{server_name}' failed to run tool '{tool_name}'")
        })?;

        Ok(render_tool_result(result))
    }

    /// Returns the running connection for `name`, connecting lazily (and
    /// caching the result) on first use. The lock is never held across the
    /// connect itself (spawning a child process or doing an HTTP handshake,
    /// both real wall-clock I/O) — only to check/populate the cache — so
    /// first-time connections to independent servers (e.g. two `parallel`
    /// branches each first-using a different server) proceed concurrently
    /// instead of queuing behind one another. A race where two callers both
    /// connect to the same new server at once is resolved by keeping
    /// whichever one wins the final cache insert and dropping the other.
    async fn connection(&self, name: &str) -> Result<Arc<RunningService<RoleClient, ()>>> {
        if let Some(existing) = self.connections.lock().await.get(name) {
            return Ok(existing.clone());
        }
        let server = self.servers.get(name).ok_or_else(|| {
            anyhow!(
                "unknown MCP server '{name}'; define it under 'mcp_servers:' in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
        let transport = server.resolve_transport(name)?;
        let running = Arc::new(connect(name, transport).await?);
        let running = self
            .connections
            .lock()
            .await
            .entry(name.to_owned())
            .or_insert(running)
            .clone();
        Ok(running)
    }
}

/// Opens one MCP connection over the given transport, using the default
/// (do-nothing) `ClientHandler` — lait only ever calls tools, so it never
/// needs to answer server-initiated requests (sampling, roots, elicitation).
async fn connect(
    name: &str,
    transport: config::McpTransport,
) -> Result<RunningService<RoleClient, ()>> {
    match transport {
        config::McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let mut process_command = tokio::process::Command::new(&command);
            process_command.args(&args).envs(&env);
            if let Some(cwd) = &cwd {
                process_command.current_dir(cwd);
            }
            let child = TokioChildProcess::new(process_command).with_context(|| {
                format!("failed to spawn MCP server '{name}' (command '{command}')")
            })?;
            ().serve(child)
                .await
                .map_err(|error| anyhow!("failed to initialize MCP server '{name}': {error}"))
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
            let transport_config =
                StreamableHttpClientTransportConfig::with_uri(url).custom_headers(header_map);
            let transport = StreamableHttpClientTransport::from_config(transport_config);
            ().serve(transport)
                .await
                .map_err(|error| anyhow!("failed to initialize MCP server '{name}': {error}"))
        }
    }
}

/// Lists every tool a server exposes, following `next_cursor` pagination
/// until the server reports none left.
async fn list_all_tools(connection: &RunningService<RoleClient, ()>) -> Result<Vec<Tool>> {
    let mut tools = Vec::new();
    let mut cursor = None;
    loop {
        let params = cursor
            .take()
            .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let result = connection
            .list_tools(params)
            .await
            .map_err(|error| anyhow!("{error}"))?;
        tools.extend(result.tools);
        match result.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(tools)
}

/// OpenAI function names must match `^[a-zA-Z0-9_-]{1,64}$`. Qualifies
/// `tool` with its `server` (so two servers' same-named tool don't collide)
/// by joining them with `__`, replacing any other character with `_`, and
/// rejecting (rather than truncating, which risks a silent second collision)
/// a result over 64 characters.
fn qualify_tool_name(server: &str, tool: &str) -> Result<String> {
    let raw = format!("{server}__{tool}");
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
            "MCP tool name '{qualified}' (from server '{server}', tool '{tool}') is empty or exceeds OpenAI's 64-character function name limit"
        );
    }
    Ok(qualified)
}

/// Renders a `tools/call` result as plain text for a `tool`-role message:
/// text content blocks joined as-is, any other block type (image/audio/
/// resource) JSON-serialized so nothing is silently dropped. `is_error` is
/// not treated specially — the model sees the error content and decides how
/// to react, the same way a real assistant sees a failed shell command.
/// Takes `result` by value (the caller never reuses it) so a text block's
/// content can be moved into the output instead of cloned — tool output can
/// be large (file contents, search results, ...).
fn render_tool_result(result: rmcp::model::CallToolResult) -> String {
    if result.content.is_empty() {
        return String::new();
    }
    result
        .content
        .into_iter()
        .map(|block| match block {
            rmcp::model::ContentBlock::Text(text) => text.text,
            other => serde_json::to_string(&other).unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::qualify_tool_name;

    #[test]
    fn qualifies_a_tool_name_with_its_server() {
        assert_eq!(
            qualify_tool_name("filesystem", "read_file").unwrap(),
            "filesystem__read_file"
        );
    }

    #[test]
    fn sanitizes_invalid_characters() {
        assert_eq!(
            qualify_tool_name("my server", "tool.name").unwrap(),
            "my_server__tool_name"
        );
    }

    #[test]
    fn rejects_a_name_over_64_characters() {
        let long_tool = "a".repeat(60);
        assert!(qualify_tool_name("server", &long_tool).is_err());
    }

    /// A hand-rolled streamable-HTTP MCP server: routes on the JSON-RPC
    /// `method` field (something `tests/support::MockServer` can't do, since
    /// it just replays canned bodies in order) and answers `initialize` /
    /// `notifications/initialized` / `tools/list` / `tools/call` — the exact
    /// four requests one `McpRegistry::tools` + `McpRegistry::call` round
    /// trip makes. This is the one live test validating the rmcp 3.1.4 API
    /// usage in this file actually round-trips over the wire; everything
    /// else in this module is exercised only against parsed/constructed
    /// values, never a real connection.
    fn start_mock_mcp_server() -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock MCP server");
        let addr = listener
            .local_addr()
            .expect("failed to read mock server address");
        let handle = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().expect("failed to accept connection");
                let mut buf = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let read = stream.read(&mut chunk).expect("failed to read request");
                    assert!(read > 0, "connection closed before headers were complete");
                    buf.extend_from_slice(&chunk[..read]);
                    if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                let content_length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while buf.len() < header_end + content_length {
                    let mut chunk = [0u8; 4096];
                    let read = stream
                        .read(&mut chunk)
                        .expect("failed to read request body");
                    assert!(read > 0, "connection closed before body was complete");
                    buf.extend_from_slice(&chunk[..read]);
                }
                let body = String::from_utf8_lossy(&buf[header_end..header_end + content_length])
                    .into_owned();
                let request: serde_json::Value =
                    serde_json::from_str(&body).expect("mock MCP server got non-JSON body");
                let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = request.get("id").cloned();

                let (status, response_body) = match method {
                    "initialize" => (
                        "200 OK",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": {},
                                "serverInfo": {"name": "mock-mcp", "version": "0.0.1"}
                            }
                        })
                        .to_string(),
                    ),
                    "notifications/initialized" => ("202 Accepted", String::new()),
                    "tools/list" => (
                        "200 OK",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": "echo",
                                    "description": "echoes the input back",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {"text": {"type": "string"}}
                                    }
                                }]
                            }
                        })
                        .to_string(),
                    ),
                    "tools/call" => (
                        "200 OK",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{"type": "text", "text": "hello from mock"}]
                            }
                        })
                        .to_string(),
                    ),
                    other => panic!("mock MCP server received an unexpected method '{other}'"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len(),
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("failed to write mock response");
                stream.flush().expect("failed to flush mock response");
            }
        });
        (format!("http://{addr}/mcp"), handle)
    }

    #[tokio::test]
    async fn connects_lists_and_calls_a_tool_over_streamable_http() {
        use crate::config::McpServerConfig;
        use async_openai::types::chat::ChatCompletionTools;
        use std::collections::HashMap;

        let (url, server_thread) = start_mock_mcp_server();

        let mut servers = HashMap::new();
        servers.insert(
            "mock".to_owned(),
            McpServerConfig {
                command: None,
                args: vec![],
                env: HashMap::new(),
                cwd: None,
                url: Some(url),
                headers: HashMap::new(),
            },
        );
        let registry = super::McpRegistry::new(servers);

        let tool_set = registry
            .tools(&["mock".to_owned()])
            .await
            .expect("tools() should list the mock server's tool");
        assert_eq!(tool_set.tools.len(), 1);
        match &tool_set.tools[0] {
            ChatCompletionTools::Function(tool) => assert_eq!(tool.function.name, "mock__echo"),
            ChatCompletionTools::Custom(_) => panic!("expected a function tool"),
        }

        let result = registry
            .call(&tool_set, "mock__echo", r#"{"text":"hi"}"#)
            .await
            .expect("call() should run the mock server's tool");
        assert_eq!(result, "hello from mock");

        server_thread
            .join()
            .expect("mock MCP server thread panicked");
    }
}
