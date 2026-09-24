//! Behavioral coverage for `lait serve --mcp` (`src/mcp_server.rs`,
//! `src/app/serve.rs`): spawns the built binary as a real MCP server over
//! stdio and drives it with `rmcp`'s own client (`transport-child-process`,
//! already a dependency for lait's own MCP *client* side — see
//! `src/mcp/stdio.rs`), rather than hand-rolling JSON-RPC framing. This
//! exercises the real `tools/list`/`tools/call` path end to end, including
//! the 2026-07-28 handshake `rmcp` negotiates on its own.

mod support;

use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use support::{ConfigDirectory, MockServer, completion_body};

/// `support::test_command` builds a blocking `std::process::Command`; this
/// mirrors its environment isolation (see that function's own doc comment)
/// for the `tokio::process::Command` an async MCP client transport needs
/// instead.
fn serve_command(config_dir: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_lait"));
    command.args(["serve", "--mcp"]).current_dir(config_dir);
    for variable in [
        "LLM_MODEL",
        "OPENAI_BASE_URL",
        "OPENAI_API_KEY",
        "LLM_REASONING_EFFORT",
    ] {
        command.env_remove(variable);
    }
    command.env(
        "XDG_CONFIG_HOME",
        std::env::temp_dir().join("lait-test-no-global-config"),
    );
    command.env(
        "XDG_DATA_HOME",
        support::next_temp_path("lait-test-data", ""),
    );
    command
}

fn tool_call(name: &str, arguments: serde_json::Value) -> CallToolRequestParams {
    // `#[non_exhaustive]`: no struct-literal construction (not even with
    // `..Default::default()`) from outside rmcp's own crate, so this builds
    // via `Default::default()` and public-field assignment instead.
    let mut params = CallToolRequestParams::default();
    params.name = name.to_owned().into();
    params.arguments = arguments.as_object().cloned();
    params
}

#[tokio::test]
async fn serve_mcp_exposes_and_runs_configured_agents_and_workflows() {
    let server = MockServer::start_sequence(&[
        ("200 OK", &completion_body("test-model", "hello, Tokyo")),
        ("200 OK", &completion_body("test-model", "workflow said hi")),
    ]);
    let config = ConfigDirectory::new(&format!(
        "base_url: \"{}\"\ndefault:\n  model: test-model\nagents:\n  greeter: agent.md\nworkflows:\n  echo_wf: workflow.yml\n",
        server.base_url
    ));
    config.write(
        "agent.md",
        "---\nname: greeter\ndescription: Greets a city\n---\nSay hello to {{ input }}.\n",
    );
    config.write(
        "workflow.yml",
        "default:\n  model: test-model\nsteps:\n  - id: greet\n    prompt: \"{{ input }}\"\n",
    );

    let transport = TokioChildProcess::new(serve_command(config.path()))
        .expect("failed to spawn 'lait serve --mcp'");
    let client =
        ().serve(transport)
            .await
            .expect("MCP client should connect to the served process");

    let tools = client
        .list_tools(None)
        .await
        .expect("tools/list should succeed");
    let mut tool_names: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    tool_names.sort_unstable();
    assert_eq!(tool_names, vec!["agent__greeter", "workflow__echo_wf"]);

    let agent_tool = tools
        .tools
        .iter()
        .find(|tool| tool.name == "agent__greeter")
        .expect("agent__greeter should be listed");
    assert_eq!(agent_tool.description.as_deref(), Some("Greets a city"));

    let agent_result = client
        .call_tool(tool_call(
            "agent__greeter",
            serde_json::json!({"input": "Tokyo"}),
        ))
        .await
        .expect("agent__greeter call should succeed");
    assert_eq!(agent_result.is_error, Some(false));
    assert!(
        agent_result
            .content
            .first()
            .and_then(|block| block.as_text())
            .is_some_and(|text| text.text.contains("hello, Tokyo")),
        "{agent_result:?}"
    );

    let workflow_result = client
        .call_tool(tool_call(
            "workflow__echo_wf",
            serde_json::json!({"input": "hi"}),
        ))
        .await
        .expect("workflow__echo_wf call should succeed");
    assert_eq!(workflow_result.is_error, Some(false));
    assert!(
        workflow_result
            .content
            .first()
            .and_then(|block| block.as_text())
            .is_some_and(|text| text.text.contains("workflow said hi")),
        "{workflow_result:?}"
    );

    server.receive_request();
    server.receive_request();
    server.finish();

    client
        .cancel()
        .await
        .expect("client should be able to close the connection");
}

#[tokio::test]
async fn serve_mcp_rejects_an_unknown_tool_name() {
    let config = ConfigDirectory::new("default:\n  model: test-model\n");

    let transport = TokioChildProcess::new(serve_command(config.path()))
        .expect("failed to spawn 'lait serve --mcp'");
    let client =
        ().serve(transport)
            .await
            .expect("MCP client should connect to the served process");

    let error = client
        .call_tool(tool_call("agent__nope", serde_json::json!({"input": "x"})))
        .await
        .expect_err("an unknown tool name should be a protocol-level error");
    assert!(format!("{error}").contains("nope"), "{error}");

    client
        .cancel()
        .await
        .expect("client should be able to close the connection");
}

#[tokio::test]
async fn serve_mcp_skips_a_workflow_with_an_ask_node() {
    let config = ConfigDirectory::new(
        "default:\n  model: test-model\nworkflows:\n  needs_human: workflow.yml\n",
    );
    config.write(
        "workflow.yml",
        "steps:\n  - id: ask_it\n    ask: \"pick one\"\n    default: \"a\"\n",
    );

    let transport = TokioChildProcess::new(serve_command(config.path()))
        .expect("failed to spawn 'lait serve --mcp'");
    let client =
        ().serve(transport)
            .await
            .expect("MCP client should connect to the served process");

    let tools = client
        .list_tools(None)
        .await
        .expect("tools/list should succeed");
    assert!(
        tools.tools.is_empty(),
        "a workflow with an 'ask:' node must not be exposed: {tools:?}"
    );

    client
        .cancel()
        .await
        .expect("client should be able to close the connection");
}
