use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};

use crate::{
    agent::{self, AgentFile},
    config, schema,
};

/// One `agents:` entry, loaded and canonicalized once then cached for the
/// registry's lifetime (see `AgentRegistry`). `canonical_path` is kept
/// alongside `file` because a recursive subagent call (a subagent whose own
/// `subagents:` names another) needs it to detect a cycle or excessive
/// nesting the same way `WorkflowScope`/`check_workflow_nesting` do for
/// `workflow:` nodes — see `app::call_subagent_tool`.
#[derive(Debug)]
pub(crate) struct LoadedAgent {
    pub(crate) file: AgentFile,
    pub(crate) canonical_path: PathBuf,
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
    /// Qualified tool name (`agent__<name>`, see `qualify_subagent_name`) to
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

    /// Returns `name`'s `LoadedAgent`, loading (and canonicalizing) it on
    /// first use and caching the result for the registry's lifetime.
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
        let loaded = Rc::new(LoadedAgent {
            file,
            canonical_path,
        });
        self.loaded
            .borrow_mut()
            .insert(name.to_owned(), Rc::clone(&loaded));
        Ok(loaded)
    }

    /// Builds the OpenAI-shaped tool definitions for `names` (a resolved
    /// `subagents:` list, already merged through every fallback layer). Each
    /// named subagent's declared `input_schema` (if any) becomes the tool's
    /// `parameters` verbatim, so the model's arguments pass straight through
    /// as the subagent's own structured input; a subagent with no
    /// `input_schema` gets a generic single-field `{ "input": ... }` schema
    /// instead — see `app::subagent_tool_input`, the matching unwrap logic on
    /// the call side.
    pub(crate) fn tools(&self, names: &[String]) -> Result<ToolSet> {
        let mut tools = Vec::with_capacity(names.len());
        let mut index = HashMap::with_capacity(names.len());
        for name in names {
            let loaded = self.load(name)?;
            let qualified = qualify_subagent_name(name)?;
            if index.contains_key(&qualified) {
                bail!("duplicate subagent name '{name}' in 'subagents:'");
            }
            let parameters = match &loaded.file.input_schema {
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
            let description = loaded
                .file
                .description
                .clone()
                .unwrap_or_else(|| format!("Runs the '{name}' subagent for a delegated task."));
            tools.push(ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: qualified.clone(),
                    description: Some(description),
                    parameters: Some(parameters),
                    strict: None,
                },
            }));
            index.insert(qualified, name.clone());
        }
        Ok(ToolSet { tools, index })
    }
}

/// OpenAI function names must match `^[a-zA-Z0-9_-]{1,64}$` (the same
/// constraint `mcp::qualify_tool_name` enforces for MCP tools). Qualifies
/// `name` with a fixed `agent__` prefix — distinguishing a subagent tool from
/// an MCP one that happens to share the same bare name — and sanitizes/
/// length-checks the result the same way.
fn qualify_subagent_name(name: &str) -> Result<String> {
    let raw = format!("agent__{name}");
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
            "subagent tool name '{qualified}' (from subagent '{name}') is empty or exceeds \
             OpenAI's 64-character function name limit"
        );
    }
    Ok(qualified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    #[test]
    fn qualifies_a_subagent_name() {
        assert_eq!(
            qualify_subagent_name("researcher").unwrap(),
            "agent__researcher"
        );
    }

    #[test]
    fn sanitizes_invalid_characters() {
        assert_eq!(
            qualify_subagent_name("my agent").unwrap(),
            "agent__my_agent"
        );
    }

    #[test]
    fn rejects_a_name_over_64_characters() {
        let long_name = "a".repeat(60);
        assert!(qualify_subagent_name(&long_name).is_err());
    }

    #[test]
    fn errors_on_an_unknown_subagent_name() {
        let agents_map: config::AgentMap = StdHashMap::new();
        let registry = AgentRegistry::new(&agents_map);
        let error = registry.load("missing").unwrap_err();
        assert!(error.to_string().contains("missing"));
    }
}
