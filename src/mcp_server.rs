//! `lait serve --mcp`'s MCP (Model Context Protocol) *server* side: exposes
//! every `agents:`/`workflows:` entry in `lait.config.yml` as one callable
//! MCP tool each, over whichever transport `app::serve` connects (currently
//! stdio only). Deliberately separate from `crate::mcp` (an MCP *client*,
//! for `mcp_servers:`) rather than a submodule of it — that module's own doc
//! comment frames it as "MCP client" throughout, and the two share almost no
//! code beyond `mcp::qualify_tool_name` (reused here so an agent/workflow
//! tool name is sanitized/length-checked exactly like an MCP server's own
//! tool names are on the client side).
//!
//! A served run bypasses `report::emit_run_output`/`finish_prompt_or_agent_run`
//! entirely (the tool result text is handed straight to the MCP client, in
//! memory) — so, unlike every other `lait` entry point, a call here never
//! writes to `lait history` and never prints a `--show-usage` summary. This
//! is a deliberate v1 boundary, not an oversight.
//!
//! # Why an `ask:` step is refused
//!
//! stdout/stdin are the MCP JSON-RPC channel here (see `app::serve`), not a
//! human terminal. A workflow's `ask:` step (`workflow::ask::run_ask`)
//! already guards itself against a non-interactive stdin (it falls back to
//! `default:`/errors instead of reading), so the common case — a real MCP
//! client, whose spawned child process never gets a tty for stdin — is safe
//! on its own. But a workflow exposed here is refused outright if any of its
//! steps (at any nesting depth within the file) is an `ask:` step, so a user
//! manually running `lait serve --mcp` from an interactive shell (where
//! stdin *is* a tty) cannot accidentally race `workflow::ask::run_ask`'s
//! blocking read against `rmcp`'s own JSON-RPC reader on the same file
//! descriptor. This check only looks at the workflow file itself — a nested
//! `workflow:` step's *own* `ask:` step, in a different file, is not caught.
//! See docs/usage/ja/serve.md.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, anyhow};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
};
use tokio_util::sync::CancellationToken;

use crate::{
    engine::{self, AppServices, RunContext},
    mcp, report, template,
    workflow::{
        self,
        exec::{Frame, run_document},
        inputs,
    },
};

/// Which config entry a qualified tool name (`agent__<name>`/
/// `workflow__<name>`, see `mcp::qualify_tool_name`) dispatches to.
enum ToolTarget {
    Agent(String),
    Workflow(PathBuf),
}

/// The MCP server `lait serve --mcp` runs. Its tool catalog (`tools`/
/// `dispatch`) is built once, at startup (see `build`) — an `agents:`/
/// `workflows:` entry added to `lait.config.yml` after the server started is
/// not picked up without a restart, matching every other `lait` command's
/// "config is read once per invocation" behavior. A workflow's own
/// `WorkflowFile`/`WorkflowScope` are deliberately *not* cached here (unlike
/// `agents:`, which reuses the existing `AppServices::agent_registry` cache):
/// each `tools/call` for a `workflow__<name>` tool re-reads and re-parses
/// that YAML file fresh. A tool call's own model round-trips cost seconds;
/// re-parsing a workflow file is noise next to that, and skipping the cache
/// avoids having to reason about sharing one `WorkflowScope` across
/// concurrent calls from the same client.
pub(crate) struct LaitMcpServer {
    services: Arc<AppServices>,
    tools: Vec<Tool>,
    dispatch: HashMap<String, ToolTarget>,
}

impl LaitMcpServer {
    /// Loads every `agents:`/`workflows:` entry once, building this server's
    /// fixed tool catalog. An entry that fails to load, whose qualified name
    /// doesn't fit OpenAI's 64-character tool-name limit (see
    /// `mcp::qualify_tool_name`), or — for a workflow — reaches an `ask:`
    /// node (see this module's doc comment) is skipped with a
    /// `report::note` rather than failing the whole server — `lait
    /// workflow list`/`lait skill list` are equally lenient about one bad
    /// registry entry.
    pub(crate) async fn build(
        services: Arc<AppServices>,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        let mut tools = Vec::new();
        let mut dispatch = HashMap::new();

        let agent_names: Vec<String> = services.file_config.agents.keys().cloned().collect();
        for name in agent_names {
            match build_agent_tool(&services, &name, cancellation.clone()).await {
                Ok((qualified, tool)) => {
                    tools.push(tool);
                    dispatch.insert(qualified, ToolTarget::Agent(name));
                }
                Err(error) => report::note(format_args!(
                    "skipping agent '{name}' as an MCP tool: {error:#}"
                )),
            }
        }

        let workflow_entries: Vec<(String, PathBuf)> = services
            .file_config
            .workflows
            .iter()
            .map(|(name, path)| (name.clone(), path.clone()))
            .collect();
        for (name, path) in workflow_entries {
            match build_workflow_tool(&name, &path, cancellation.clone()).await {
                Ok(Some((qualified, tool))) => {
                    tools.push(tool);
                    dispatch.insert(qualified, ToolTarget::Workflow(path));
                }
                // Already reported (with its own, more specific reason) by
                // `build_workflow_tool` itself.
                Ok(None) => {}
                Err(error) => report::note(format_args!(
                    "skipping workflow '{name}' as an MCP tool: {error:#}"
                )),
            }
        }

        Ok(Self {
            services,
            tools,
            dispatch,
        })
    }

    /// Every qualified tool name this server exposes, for `app::serve`'s
    /// startup note.
    pub(crate) fn tool_names(&self) -> impl Iterator<Item = &str> {
        self.dispatch.keys().map(String::as_str)
    }

    async fn call_agent_tool(
        &self,
        name: &str,
        arguments_json: &str,
        cancellation: CancellationToken,
    ) -> Result<String> {
        let env = RunContext::new(Arc::clone(&self.services), cancellation.clone());
        engine::call_subagent_tool(name, arguments_json, &env, &[], cancellation).await
    }

    async fn call_workflow_tool(
        &self,
        path: &std::path::Path,
        arguments_json: &str,
        cancellation: CancellationToken,
    ) -> Result<String> {
        let (input, provided) = workflow_tool_input(arguments_json)?;
        let wf = workflow::load_workflow_cancellable(path, cancellation.clone()).await?;
        let resolved = inputs::resolve(&wf.inputs, inputs::from_object(provided))?;
        let initial = match input {
            Some(text) => {
                inputs::resolve_initial(&wf, inputs::InitialInput::Text(text), cancellation.clone())
                    .await?
            }
            None if wf.declares_inputs() => serde_json::Value::Null,
            None => anyhow::bail!("workflow tool call is missing the required 'input' field"),
        };
        let scope =
            workflow::WorkflowScope::top_level(&wf, path, resolved, cancellation.clone()).await?;
        let env = RunContext::new(Arc::clone(&self.services), cancellation.clone());
        let (output, _) =
            run_document(&wf, initial, Frame::new(&scope, &env, cancellation)).await?;
        Ok(template::to_text(&output))
    }
}

/// `name`'s tool metadata (its qualified name and OpenAI/MCP-shaped `Tool`
/// definition) — `AgentRegistry::load_cancellable` already computes exactly
/// the `parameters`/description this needs for the internal
/// subagent-tool-call path (see its own doc comment), so this just qualifies
/// the name and reuses that.
async fn build_agent_tool(
    services: &AppServices,
    name: &str,
    cancellation: CancellationToken,
) -> Result<(String, Tool)> {
    let loaded = services
        .agent_registry
        .load_cancellable(name, cancellation)
        .await?;
    let qualified = mcp::qualify_tool_name("agent", "agent", name)?;
    let description = loaded
        .file
        .description
        .clone()
        .unwrap_or_else(|| format!("Run the '{name}' agent."));
    let parameters = loaded
        .tool_parameters()
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("agent '{name}'s input schema must be a JSON object"))?;
    Ok((
        qualified.clone(),
        Tool::new(qualified, description, parameters),
    ))
}

/// Returns `Ok(None)` (already reported via `report::note`) for a workflow
/// this server declines to expose (an `ask:` step — see this module's doc
/// comment); `Err` for a load/parse/qualify failure, which the caller
/// reports itself so every skip reason is worded consistently.
async fn build_workflow_tool(
    name: &str,
    path: &std::path::Path,
    cancellation: CancellationToken,
) -> Result<Option<(String, Tool)>> {
    let wf = workflow::load_workflow_cancellable(path, cancellation)
        .await
        .with_context(|| format!("failed to load workflow '{}'", path.display()))?;
    if contains_ask(&wf.steps) {
        report::note(format_args!(
            "skipping workflow '{name}' as an MCP tool: it has an 'ask:' step, which reads \
             stdin — unsafe under 'lait serve --mcp' (see docs/usage/ja/serve.md)"
        ));
        return Ok(None);
    }
    let qualified = mcp::qualify_tool_name("workflow", "workflow", name)?;
    let description = wf
        .description
        .clone()
        .unwrap_or_else(|| format!("Run the '{name}' workflow."));
    Ok(Some((
        qualified.clone(),
        Tool::new(qualified, description, workflow_input_schema(&wf)),
    )))
}

/// Whether any step in `steps`, at any nesting depth within this file, is
/// an `ask:` step — see this module's doc comment.
fn contains_ask(steps: &[workflow::Step]) -> bool {
    steps.iter().any(|step| {
        if matches!(step.kind, workflow::StepKind::Ask(_)) {
            return true;
        }
        let mut found = false;
        step.for_each_child(|_, children| found = found || contains_ask(children));
        found
    })
}

/// The MCP tool schema for a `workflow__<name>` tool: an `input` string
/// matching `lait run <name> <PROMPT>`'s own positional argument (required
/// unless the workflow declares `inputs:`, the same rule `lait run` applies),
/// and an `inputs` object carrying the workflow's declared `inputs:` — each
/// entry's own JSON Schema verbatim, required unless it has a `default`.
fn workflow_input_schema(
    wf: &workflow::WorkflowFile,
) -> serde_json::Map<String, serde_json::Value> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "input".to_owned(),
        serde_json::json!({
            "type": "string",
            "description": "The workflow's initial value ({{ input }} of its first step): a \
                plain-text string, or JSON text if the workflow's 'input_schema' expects \
                structured input."
        }),
    );
    let mut required = Vec::new();
    if wf.declares_inputs() {
        let mut input_properties = serde_json::Map::new();
        let mut required_inputs = Vec::new();
        for (name, definition) in &wf.inputs {
            let mut schema = definition.schema.clone();
            if let (Some(description), Some(object)) =
                (&definition.description, schema.as_object_mut())
            {
                object
                    .entry("description")
                    .or_insert_with(|| serde_json::Value::String(description.clone()));
            }
            input_properties.insert(name.clone(), schema);
            if definition.default.is_none() {
                required_inputs.push(serde_json::Value::String(name.clone()));
            }
        }
        properties.insert(
            "inputs".to_owned(),
            serde_json::json!({
                "type": "object",
                "description": "Values for the workflow's declared 'inputs:'.",
                "properties": input_properties,
                "required": required_inputs,
            }),
        );
        if !required_inputs.is_empty() {
            required.push(serde_json::Value::String("inputs".to_owned()));
        }
    } else {
        required.push(serde_json::Value::String("input".to_owned()));
    }
    let mut schema = serde_json::Map::new();
    schema.insert("type".to_owned(), "object".into());
    schema.insert("properties".to_owned(), properties.into());
    schema.insert("required".to_owned(), required.into());
    schema
}

/// Unwraps a `workflow__<name>` tool call's raw JSON `arguments` into the
/// initial input text and declared-input values `call_workflow_tool` runs the
/// workflow with — the `{"input": ..., "inputs": {...}}` shape
/// `workflow_input_schema` declares, mirrored on the read side.
/// `value_to_input_text` (shared with `engine::agent`'s own subagent-tool
/// unwrapping) passes a JSON string through unquoted and serializes any
/// other JSON value to compact text.
fn workflow_tool_input(
    arguments_json: &str,
) -> Result<(Option<String>, serde_json::Map<String, serde_json::Value>)> {
    let arguments: serde_json::Value = if arguments_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments_json)
            .context("failed to parse workflow tool call arguments as JSON")?
    };
    let serde_json::Value::Object(mut arguments) = arguments else {
        anyhow::bail!("workflow tool call arguments must be a JSON object");
    };
    let input = arguments
        .remove("input")
        .map(|value| {
            engine::value_to_input_text(&value, "failed to serialize workflow tool call 'input'")
        })
        .transpose()?;
    let provided = match arguments.remove("inputs") {
        None | Some(serde_json::Value::Null) => serde_json::Map::new(),
        Some(serde_json::Value::Object(map)) => map,
        Some(_) => anyhow::bail!("workflow tool call 'inputs' must be a JSON object"),
    };
    Ok((input, provided))
}

impl ServerHandler for LaitMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            rmcp::model::Implementation::new("lait", env!("CARGO_PKG_VERSION"))
                .with_description("lait's configured workflows/agents, exposed as MCP tools"),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tools.clone(),
            ..Default::default()
        })
    }

    /// Dispatches by the tool name the client asked for, using `context.ct`
    /// (cancelled on the MCP-level `notifications/cancelled` for *this*
    /// request specifically — a per-request signal `rmcp` gives us for
    /// free, distinct from the whole server's own shutdown token in
    /// `app::serve`) as this one call's cancellation.
    ///
    /// A failure while actually running the agent/workflow becomes a
    /// tool-level error (`CallToolResult::error`, still an `Ok` at the
    /// protocol level) rather than a protocol-level `Err`: the request
    /// routed correctly and the caller should see why it failed, exactly
    /// the distinction `CallToolResult::error`'s own doc comment draws.
    /// An unknown tool name is the one case that *is* a protocol error — the
    /// request could not be routed at all.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(target) = self.dispatch.get(request.name.as_ref()) else {
            return Err(McpError::invalid_params(
                format!("unknown tool '{}'", request.name),
                None,
            ));
        };
        let arguments_json = serde_json::to_string(&request.arguments.unwrap_or_default())
            .map_err(|error| {
                McpError::internal_error(
                    format!("failed to encode tool call arguments: {error}"),
                    None,
                )
            })?;
        let cancellation = context.ct.clone();
        let result = match target {
            ToolTarget::Agent(name) => {
                self.call_agent_tool(name, &arguments_json, cancellation)
                    .await
            }
            ToolTarget::Workflow(path) => {
                self.call_workflow_tool(path, &arguments_json, cancellation)
                    .await
            }
        };
        Ok(match result {
            Ok(output) => CallToolResult::success(vec![ContentBlock::text(output)]).into(),
            Err(error) => {
                CallToolResult::error(vec![ContentBlock::text(format!("{error:#}"))]).into()
            }
        })
    }
}
