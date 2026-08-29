use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};

use crate::{
    agent::{self, AgentFile},
    config, mcp, schema,
};

/// One `agents:` entry, loaded and canonicalized once then cached for the
/// registry's lifetime (see `AgentRegistry`). `canonical_path` is kept
/// alongside `file` because a recursive subagent call (a subagent whose own
/// `subagents:` names another) needs it to detect a cycle or excessive
/// nesting the same way `WorkflowScope`/`check_workflow_nesting` do for
/// `workflow:` nodes — see `app::call_subagent_tool`. `tool_parameters`/
/// `tool_description` are resolved once here too (not rebuilt by
/// `AgentRegistry::tools` on every call): `tool_parameters` resolves
/// `file.input_schema`, which for a `file_path:` entry means reading and
/// parsing a JSON file — real I/O that a `for_each`/`loop` workflow node
/// with `subagents:` set would otherwise repeat on every iteration, the same
/// waste `mcp::McpRegistry`'s own `tool_lists` cache avoids for MCP tools.
#[derive(Debug)]
pub(crate) struct LoadedAgent {
    pub(crate) file: AgentFile,
    pub(crate) canonical_path: PathBuf,
    tool_parameters: serde_json::Value,
    tool_description: String,
}

/// A named agent Markdown file made available as a callable "subagent" tool
/// (see `agent::load_agent`), resolved from `lait.config.yml`'s top-level
/// `agents:` map. Mirrors `skill::SkillCache`: agent files are loaded lazily
/// (parsing never sees the config file) and cached for the registry's
/// lifetime, since a subagent's definition doesn't change over the course of
/// one `lait run`/`lait agent run`/chat invocation. Uses `RefCell`/`Rc`
/// rather than `tokio::sync::Mutex`/`Arc` like `mcp::McpRegistry`, for the
/// same reason as `SkillCache`: loading a file is synchronous, so there is no
/// `.await` to interleave across when `parallel:`/`for_each:` branches (or
/// concurrent tool calls within one round — see
/// `app::RequestSettings::complete`) race on the same name.
pub(crate) struct AgentRegistry<'a> {
    agents_map: &'a config::AgentMap,
    loaded: RefCell<HashMap<String, Rc<LoadedAgent>>>,
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
    /// `app::RequestSettings::complete`, which combines this with
    /// `mcp::ToolSet`).
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
    }
}

impl<'a> AgentRegistry<'a> {
    pub(crate) fn new(agents_map: &'a config::AgentMap) -> Self {
        Self {
            agents_map,
            loaded: RefCell::new(HashMap::new()),
        }
    }

    /// Returns `name`'s `LoadedAgent`, loading (and canonicalizing) it, and
    /// resolving its tool `parameters`/`description`, on first use, then
    /// caching the result for the registry's lifetime. A named subagent's
    /// declared `input_schema` (if any) becomes the tool's `parameters`
    /// verbatim, so the model's arguments pass straight through as the
    /// subagent's own structured input; a subagent with no `input_schema`
    /// gets a generic single-field `{ "input": ... }` schema instead — see
    /// `app::subagent_tool_input`, the matching unwrap logic on the call
    /// side.
    pub(crate) fn load(&self, name: &str) -> Result<Rc<LoadedAgent>> {
        if let Some(cached) = self.loaded.borrow().get(name) {
            return Ok(Rc::clone(cached));
        }
        let path = self.agents_map.get(name).ok_or_else(|| {
            anyhow!(
                "unknown subagent '{name}'; define it under 'agents:' in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
        let file = agent::load_agent(path)?;
        let canonical_path = std::fs::canonicalize(path).with_context(|| {
            format!(
                "failed to resolve subagent '{name}' file path '{}'",
                path.display()
            )
        })?;
        let tool_parameters = match &file.input_schema {
            Some(entry) => schema::load_schema_value(entry)?,
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
        let tool_description = file
            .description
            .clone()
            .unwrap_or_else(|| format!("Runs the '{name}' subagent for a delegated task."));
        let loaded = Rc::new(LoadedAgent {
            file,
            canonical_path,
            tool_parameters,
            tool_description,
        });
        self.loaded
            .borrow_mut()
            .insert(name.to_owned(), Rc::clone(&loaded));
        Ok(loaded)
    }

    /// Builds the OpenAI-shaped tool definitions for `names` (a resolved
    /// `subagents:` list, already merged through every fallback layer), from
    /// each named subagent's cached `LoadedAgent` (see `load`).
    pub(crate) fn tools(&self, names: &[String]) -> Result<ToolSet> {
        let mut tools = Vec::with_capacity(names.len());
        let mut index = HashMap::with_capacity(names.len());
        for name in names {
            let loaded = self.load(name)?;
            let qualified = mcp::qualify_tool_name("subagent tool", "agent", name)?;
            if index.contains_key(&qualified) {
                bail!("duplicate subagent name '{name}' in 'subagents:'");
            }
            tools.push(ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: qualified.clone(),
                    description: Some(loaded.tool_description.clone()),
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

    #[test]
    fn errors_on_an_unknown_subagent_name() {
        let agents_map: config::AgentMap = StdHashMap::new();
        let registry = AgentRegistry::new(&agents_map);
        let error = registry.load("missing").unwrap_err();
        assert!(error.to_string().contains("missing"));
    }
}
