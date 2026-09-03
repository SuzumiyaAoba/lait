//! Custom shell tools defined in `lait.config.yml`'s top-level `tools:` map
//! (see `config::ShellToolDefinition`): a local command exposed to the model
//! as a callable tool without standing up an MCP server. Enabled the same
//! way `mcp:`/`skills:`/`subagents:` are (a `tools:` list on the CLI/agent
//! file/workflow node, or `default.tools`), gated by the same
//! `tool_policy`/`--approve-tools` `engine::execute_tool_calls` already
//! enforces for MCP/subagent tools — the qualified name this module produces
//! (see `tools` below) flows through that same string-keyed dispatch, so
//! neither gate needed a shell-tool-specific case.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};

use crate::{config, mcp, process, template};

/// How long a tool's command may run before it's killed and the call fails,
/// when its `tools:` entry sets no `timeout:` of its own.
pub(crate) const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 30;

/// The shell-tool half of `mcp::ToolSet`/`subagent::ToolSet`: an OpenAI tool
/// list plus an index from each tool's qualified name (`tool__<name>`, see
/// `mcp::qualify_tool_name`) back to the plain `tools:` name `call` needs to
/// look the definition back up.
#[derive(Debug)]
pub(crate) struct ToolSet {
    pub(crate) tools: Vec<ChatCompletionTools>,
    index: HashMap<String, String>,
}

impl ToolSet {
    /// The `tools:` name `qualified_name` (as returned in this set's
    /// `tools`) refers to, if any.
    pub(crate) fn tool_name(&self, qualified_name: &str) -> Option<&str> {
        self.index.get(qualified_name).map(String::as_str)
    }

    /// Every qualified tool name this set defines, used only to check for a
    /// collision against the other tool sources — see
    /// `engine::RequestSettings::complete`/`complete_stream`.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
    }
}

/// Resolves `names` (a request's `tools:` list) against `tools_map`
/// (`file_config.tools`) into a [`ToolSet`], validating each referenced
/// definition (see `config::check_shell_tool_definition`) as it's turned
/// into an OpenAI tool schema — an invalid definition only ever errors for a
/// request that actually names it, the same way an invalid `models:` entry
/// only errors for a request that resolves it (`lait lint` is where every
/// entry, referenced or not, gets checked).
pub(crate) fn tools(names: &[String], tools_map: &config::ToolMap) -> Result<ToolSet> {
    let mut tools = Vec::with_capacity(names.len());
    let mut index = HashMap::with_capacity(names.len());
    for name in names {
        let definition = tools_map.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown tool '{name}'; define it under 'tools:' in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
        config::check_shell_tool_definition(name, definition)?;
        let qualified = mcp::qualify_tool_name("shell tool", "tool", name)?;
        if index.contains_key(&qualified) {
            bail!("duplicate tool name '{name}' in 'tools:'");
        }
        tools.push(ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: qualified.clone(),
                description: definition.description.clone(),
                parameters: Some(definition.parameters.clone()),
                strict: None,
            },
        }));
        index.insert(qualified, name.clone());
    }
    Ok(ToolSet { tools, index })
}

/// Renders `definition.command`'s elements as handlebars templates (see
/// `template::render`) against `input` (the model's call arguments, exposed
/// as `{{ input.<field> }}` — the same `input`/dotted-field access a
/// workflow's own templates use), producing the argv `call` execs. Also used
/// by `preview_argv`, for `--approve-tools`'s confirmation prompt.
fn render_argv(
    definition: &config::ShellToolDefinition,
    input: &serde_json::Value,
) -> Result<Vec<String>> {
    let empty_steps = serde_json::Map::new();
    let empty_vars = serde_json::Map::new();
    definition
        .command
        .iter()
        .map(|part| template::render(part, input, &empty_steps, &empty_vars))
        .collect()
}

/// A best-effort preview of the argv `call` would actually execute for
/// `arguments_json`, for `--approve-tools`'s confirmation prompt (see
/// `engine::tool_decision`) — a user approving a shell tool call needs to see
/// the real command, not just the model's raw JSON arguments, since a
/// `command:` template can transform them arbitrarily (e.g. splicing a path
/// into a larger shell one-liner). `None` when the arguments/template can't
/// be rendered (invalid JSON, a call that omits a field a template
/// references) rather than erroring — the prompt still shows the raw
/// arguments in that case, and the real failure (as an `Ok` result string,
/// or a hard error for unparseable JSON) surfaces from `call` itself if the
/// call is actually allowed to run.
pub(crate) fn preview_argv(
    definition: &config::ShellToolDefinition,
    arguments_json: &str,
) -> Option<String> {
    let input: serde_json::Value = if arguments_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments_json).ok()?
    };
    let argv = render_argv(definition, &input).ok()?;
    Some(format!("{argv:?}"))
}

/// Runs `definition`'s command for one tool call — see `render_argv` for how
/// `arguments_json` becomes the argv execed directly, never through a shell.
///
/// An unknown tool name or call arguments that aren't valid JSON both bail!,
/// failing the whole round — a model/config mistake worth surfacing loudly,
/// matching how `McpRegistry::call` treats the same two cases. Everything
/// past that point — a `command:` template that fails to render because the
/// call omitted a field it references, a non-zero exit, a timeout — is
/// always turned into an `Ok` result text instead (mirroring how an MCP
/// `tools/call` failure is rendered as text, not propagated), so neither a
/// template mismatch nor a failed command aborts the tool loop — the model
/// sees what went wrong and can try something else.
pub(crate) async fn call(
    definition: &config::ShellToolDefinition,
    arguments_json: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let input: serde_json::Value = if arguments_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments_json).context("tool call arguments must be a JSON object")?
    };
    let argv = match render_argv(definition, &input) {
        Ok(argv) => argv,
        Err(error) => return Ok(format!("tool command failed: {error:#}")),
    };

    let timeout_secs = definition.timeout.unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS);
    let child_cancel = cancellation
        .as_ref()
        .map(tokio_util::sync::CancellationToken::child_token)
        .unwrap_or_default();
    let mut execution = Box::pin(process::run_command(&argv, "", Some(child_cancel.clone())));
    let outcome =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), &mut execution)
            .await
        {
            Ok(result) => result,
            Err(_) => {
                child_cancel.cancel();
                let _ = execution.await;
                Err(anyhow::anyhow!(
                    "command timed out after {timeout_secs}s: {argv:?}"
                ))
            }
        };
    match outcome {
        Ok(output) => Ok(output),
        Err(error) => Ok(format!("tool command failed: {error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    fn definition(command: &[&str]) -> config::ShellToolDefinition {
        serde_yaml::from_str(&format!(
            "command: [{}]\n",
            command
                .iter()
                .map(|part| format!("{part:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .expect("fixture tool definition should deserialize")
    }

    #[test]
    fn errors_on_an_unknown_tool_name() {
        let tools_map: config::ToolMap = StdHashMap::new();
        let error = tools(&["missing".to_owned()], &tools_map).unwrap_err();
        assert!(error.to_string().contains("unknown tool"));
    }

    #[test]
    fn qualifies_tool_names_with_the_tool_prefix() {
        let mut tools_map: config::ToolMap = StdHashMap::new();
        tools_map.insert("echo".to_owned(), definition(&["echo"]));
        let tool_set = tools(&["echo".to_owned()], &tools_map).unwrap();
        assert_eq!(tool_set.tool_name("tool__echo"), Some("echo"));
    }

    #[tokio::test]
    async fn renders_command_arguments_from_the_call_arguments() {
        let definition = definition(&["echo", "{{ input.text }}"]);
        let output = call(&definition, r#"{"text":"hi there"}"#, None)
            .await
            .unwrap();
        assert_eq!(output.trim(), "hi there");
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_reported_as_a_result_string_not_an_error() {
        let definition = definition(&["sh", "-c", "exit 3"]);
        let output = call(&definition, "{}", None).await.unwrap();
        assert!(output.contains("tool command failed"), "output: {output}");
    }

    #[tokio::test]
    async fn a_timeout_is_reported_as_a_result_string_not_an_error() {
        let mut definition = definition(&["sh", "-c", "sleep 5"]);
        definition.timeout = Some(0);
        let output = call(&definition, "{}", None).await.unwrap();
        assert!(output.contains("timed out"), "output: {output}");
    }

    #[tokio::test]
    async fn a_template_render_failure_is_reported_as_a_result_string_not_an_error() {
        // `input.text` is never provided, so `template::render`'s strict
        // mode fails to resolve it — this must not abort the whole round
        // just because the model omitted an optional-looking field.
        let definition = definition(&["echo", "{{ input.text }}"]);
        let output = call(&definition, "{}", None).await.unwrap();
        assert!(output.contains("tool command failed"), "output: {output}");
    }

    #[test]
    fn preview_argv_renders_the_command_that_would_actually_run() {
        let definition = definition(&["echo", "{{ input.text }}"]);
        let preview = preview_argv(&definition, r#"{"text":"hi there"}"#).unwrap();
        assert_eq!(preview, r#"["echo", "hi there"]"#);
    }

    #[test]
    fn preview_argv_is_none_when_the_template_cannot_render() {
        let definition = definition(&["echo", "{{ input.text }}"]);
        assert!(preview_argv(&definition, "{}").is_none());
    }
}
