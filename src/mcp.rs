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

/// A running MCP connection: lait only ever calls tools, so it never needs
/// the more elaborate `ClientHandler`/`RoleClient` type parameters a server
/// that answered sampling/roots/elicitation requests would need.
type McpConnection = RunningService<RoleClient, ()>;

/// One server name's entry in `McpRegistry::connections`: a cell so
/// concurrent first-time callers for the same name await one shared connect
/// instead of each racing to spawn their own (see `McpRegistry::connection`).
type ConnectionCell = Arc<tokio::sync::OnceCell<Arc<McpConnection>>>;

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
    connections: tokio::sync::Mutex<HashMap<String, ConnectionCell>>,
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

impl<'a> McpRegistry<'a> {
    pub(crate) fn new(servers: &'a config::McpServerMap) -> Self {
        Self {
            servers,
            connections: tokio::sync::Mutex::new(HashMap::new()),
            tool_lists: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Connects to (or reuses an existing connection to) every server in
    /// `names`, lists their tools (or reuses a previously cached list), and
    /// returns them qualified and converted to OpenAI's `tools:` shape.
    /// Servers are connected to and listed concurrently (each is an
    /// independent round trip), not one at a time. `names` with no
    /// `mcp_servers:` entry is an error naming `lait.config.yml`'s
    /// `mcp_servers:`, since that can only be caught here (workflow/agent-file
    /// parsing never sees the config file).
    pub(crate) async fn tools(&self, names: &[String]) -> Result<ToolSet> {
        let per_server = futures_util::future::try_join_all(names.iter().map(|name| async move {
            let server_tools = self.server_tools(name).await?;
            Ok::<_, anyhow::Error>((name.clone(), server_tools))
        }))
        .await?;

        let mut tools = Vec::new();
        let mut index = HashMap::new();
        for (name, server_tools) in per_server {
            for tool in server_tools.iter() {
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
    async fn server_tools(&self, name: &str) -> Result<Arc<Vec<Tool>>> {
        let cell = self
            .tool_lists
            .lock()
            .await
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
            .clone();

        cell.get_or_try_init(|| async {
            let connection = self.connection(name).await?;
            let server_tools = list_all_tools(&connection)
                .await
                .with_context(|| format!("failed to list tools for MCP server '{name}'"))?;
            Ok::<_, anyhow::Error>(Arc::new(server_tools))
        })
        .await
        .map(Arc::clone)
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
    /// caching the result) on first use. The lock is only ever held to
    /// fetch-or-insert the `OnceCell` for `name`, never across the connect
    /// itself (spawning a child process or doing an HTTP handshake, both real
    /// wall-clock I/O) — so connections to independent servers (e.g. two
    /// `parallel` branches each first-using a different server) proceed
    /// concurrently. Concurrent callers racing on the *same* new server name
    /// share one `OnceCell` and thus one connect: `get_or_try_init` runs the
    /// connect for exactly one of them and the rest await its result, so no
    /// connection is ever established and then discarded.
    async fn connection(&self, name: &str) -> Result<Arc<McpConnection>> {
        let cell = self
            .connections
            .lock()
            .await
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
            .clone();

        cell.get_or_try_init(|| async {
            let server = self.servers.get(name).ok_or_else(|| {
                anyhow!(
                    "unknown MCP server '{name}'; define it under 'mcp_servers:' in {}",
                    config::CONFIG_FILE_NAME
                )
            })?;
            let transport = server.resolve_transport(name)?;
            Ok::<_, anyhow::Error>(Arc::new(connect(name, transport).await?))
        })
        .await
        .map(Arc::clone)
    }
}

/// Opens one MCP connection over the given transport, using the default
/// (do-nothing) `ClientHandler` — lait only ever calls tools, so it never
/// needs to answer server-initiated requests (sampling, roots, elicitation).
async fn connect(name: &str, transport: config::McpTransport) -> Result<McpConnection> {
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
async fn list_all_tools(connection: &McpConnection) -> Result<Vec<Tool>> {
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
}
