use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use tokio::sync::OnceCell;

use crate::{
    agent::{self, AgentFile},
    async_io, config, mcp, schema,
};

/// One agent file (an `agents:` entry, or a workflow node's `agent:` path),
/// loaded and canonicalized once then cached for the registry's lifetime
/// (see `AgentRegistry`). `canonical_path` is kept alongside `file` because
/// a recursive subagent call (a subagent whose own `subagents:` names
/// another) needs it to detect a cycle or excessive nesting the same way
/// `WorkflowScope`/`check_workflow_nesting` do for `workflow:` nodes — see
/// `engine::call_subagent_tool`. `tool_parameters` is resolved once here too
/// (not rebuilt by `AgentRegistry::tools` on every call): it resolves
/// `file.input_schema`, which for a `file_path:` entry means reading and
/// parsing a JSON file — real I/O that a `for_each`/`loop` workflow node
/// would otherwise repeat on every iteration, the same waste
/// `mcp::McpRegistry`'s own `tool_lists` cache avoids for MCP tools.
#[derive(Debug)]
pub(crate) struct LoadedAgent {
    pub(crate) file: AgentFile,
    pub(crate) canonical_path: PathBuf,
    tool_parameters: serde_json::Value,
}

impl LoadedAgent {
    /// Validates a subagent tool call's `input` against `file.input_schema`,
    /// using the value already resolved into `tool_parameters` instead of
    /// re-reading a `file_path:` schema from disk on every call the way
    /// `AgentFile::validate_input` does — the same repeated I/O
    /// `tool_parameters` itself was cached to avoid. A no-op when the agent
    /// has no `input_schema`, mirroring `AgentFile::validate_input`.
    pub(crate) fn validate_input(&self, input: &serde_json::Value) -> Result<()> {
        if self.file.input_schema.is_none() {
            return Ok(());
        }
        schema::validate_input_against_schema(&self.tool_parameters, input)
    }
}

/// The agent files in play for one `lait run`/`lait agent run`/chat
/// invocation: named `agents:` entries made available as callable "subagent"
/// tools (see `AgentRegistry::tools`), and workflow nodes' own `agent:`
/// paths (see `AgentRegistry::load_path`), both loaded through the same
/// cache. Mirrors `skill::SkillCache`: agent files are loaded lazily
/// (parsing never sees the config file) and cached by their configured path
/// for the registry's lifetime, since an agent file's content doesn't change
/// over the course of one invocation — without this, a `for_each`/`loop`
/// node with `agent:` set would re-read and re-parse the same file (and its
/// `file_path:` input schema) on every iteration. Each path gets its own
/// `OnceCell`, the same per-entry scheme `SkillCache` uses (see its doc
/// comment): the outer `Mutex` is only ever held long enough to fetch or
/// insert a cell, and the cell's `get_or_try_init` is what makes two
/// concurrent `parallel:`/`for_each:` branches (or concurrent tool calls
/// within one round — see `engine::RequestSettings::complete`) racing on the
/// same path share one load instead of two.
pub(crate) struct AgentRegistry {
    agents_map: Arc<config::AgentMap>,
    loaded: Mutex<HashMap<PathBuf, Arc<OnceCell<Arc<LoadedAgent>>>>>,
}

/// The OpenAI-shaped tool definitions for one completion request's
/// `subagents:` list, plus the bookkeeping needed to route a model's tool
/// call back to the subagent it names. Built fresh by `AgentRegistry::tools`
/// for every request (which subagents are in play can differ request to
/// request even though each named subagent's own definition doesn't).
pub(crate) struct ToolSet {
    pub(crate) tools: Vec<ChatCompletionTools>,
    /// Qualified tool name (`agent__<name>`, see `mcp::qualify_tool_name`) to
    /// the subagent name it calls.
    index: HashMap<String, String>,
}

impl ToolSet {
    /// The subagent name `qualified_name` (as returned in this `tools`)
    /// calls, if any.
    pub(crate) fn subagent_name(&self, qualified_name: &str) -> Option<&str> {
        self.index.get(qualified_name).map(String::as_str)
    }

    /// Every qualified tool name this set defines, used only to check for a
    /// collision against another tool source (see
    /// `engine::RequestSettings::complete`, which combines this with
    /// `mcp::ToolSet`).
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
    }
}

impl AgentRegistry {
    pub(crate) fn new(agents_map: Arc<config::AgentMap>) -> Self {
        Self {
            agents_map,
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the named subagent's `LoadedAgent`, resolving `name` against
    /// the `agents:` map. Agent files and
    /// file-backed input schemas are loaded on dedicated workers so a timed
    /// workflow step cannot get stuck in the synchronous registry cache miss.
    pub(crate) async fn load_cancellable(
        &self,
        name: &str,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Arc<LoadedAgent>> {
        let path = self.agents_map.get(name).ok_or_else(|| {
            anyhow!(
                "unknown subagent '{name}'; define it under 'agents:' in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
        self.load_path_cancellable(path, cancellation)
            .await
            .with_context(|| format!("subagent '{name}'"))
    }

    /// Returns the `LoadedAgent` for the agent file at `path` (as configured
    /// — the same path spelling always hits the same cache entry), loading
    /// and canonicalizing it, and resolving its tool `parameters`, on first
    /// use, then caching the result for the registry's lifetime. An agent's
    /// declared `input_schema` (if any) becomes the tool's `parameters`
    /// verbatim, so the model's arguments pass straight through as the
    /// subagent's own structured input; an agent with no `input_schema` gets
    /// a generic single-field `{ "input": ... }` schema instead — see
    /// `engine::subagent_tool_input`, the matching unwrap logic on the call
    /// side.
    pub(crate) async fn load_path_cancellable(
        &self,
        path: &Path,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Arc<LoadedAgent>> {
        let cell = Arc::clone(
            self.loaded
                .lock()
                .expect("agent registry lock should not be poisoned")
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(OnceCell::new())),
        );
        let loaded = cell
            .get_or_try_init(|| async {
                let (file, canonical_path) = tokio::try_join!(
                    agent::load_agent_cancellable(path, cancellation.clone()),
                    async {
                        async_io::canonicalize(path, cancellation.clone())
                            .await
                            .with_context(|| {
                                format!("failed to resolve agent file path '{}'", path.display())
                            })
                    },
                )?;
                let tool_parameters = match &file.input_schema {
                    Some(entry) => {
                        schema::load_schema_value_cancellable(entry, cancellation).await?
                    }
                    None => serde_json::json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "description": "The task or input to pass to the subagent. A string \
                                    for a plain-text task, or a JSON object/array if the subagent \
                                    expects structured input."
                            }
                        },
                        "required": ["input"],
                    }),
                };
                Ok::<_, anyhow::Error>(Arc::new(LoadedAgent {
                    file,
                    canonical_path,
                    tool_parameters,
                }))
            })
            .await?;
        Ok(Arc::clone(loaded))
    }

    /// Builds the OpenAI-shaped tool definitions for `names` (a resolved
    /// `subagents:` list, already merged through every fallback layer), from
    /// each named subagent's cached `LoadedAgent`. Tool construction itself is
    /// cheap; only the lazy registry loads need the worker path.
    pub(crate) async fn tools_cancellable(
        &self,
        names: &[String],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<ToolSet> {
        let loaded_agents = futures_util::future::try_join_all(
            names
                .iter()
                .map(|name| self.load_cancellable(name, cancellation.clone())),
        )
        .await?;

        let mut tools = Vec::with_capacity(names.len());
        let mut index = HashMap::with_capacity(names.len());
        for (name, loaded) in names.iter().zip(loaded_agents) {
            let qualified = mcp::qualify_tool_name("subagent tool", "agent", name)?;
            if index.contains_key(&qualified) {
                bail!("duplicate subagent name '{name}' in 'subagents:'");
            }
            let description = loaded
                .file
                .description
                .clone()
                .unwrap_or_else(|| format!("Runs the '{name}' subagent for a delegated task."));
            tools.push(ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: qualified.clone(),
                    description: Some(description),
                    parameters: Some(loaded.tool_parameters.clone()),
                    strict: None,
                },
            }));
            index.insert(qualified, name.clone());
        }
        Ok(ToolSet { tools, index })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    #[tokio::test]
    async fn errors_on_an_unknown_subagent_name() {
        let agents_map: config::AgentMap = StdHashMap::new();
        let registry = AgentRegistry::new(Arc::new(agents_map));
        let error = registry
            .load_cancellable("missing", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing"));
    }
}
