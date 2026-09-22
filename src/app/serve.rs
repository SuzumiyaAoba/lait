//! `lait serve --mcp`: CLI-facing orchestration — argument validation,
//! config loading, and handing off to `mcp_server::LaitMcpServer` (the
//! actual MCP protocol/tool-catalog implementation). See
//! docs/usage/ja/serve.md.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rmcp::{ServiceExt, transport::stdio};

use crate::{cli::ServeArgs, config::ConfigSource, engine::AppServices, mcp_server, report};

pub(super) async fn run(
    args: ServeArgs,
    config_source: ConfigSource,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    if !args.mcp {
        bail!("'lait serve' currently only supports '--mcp'; see docs/usage/ja/serve.md");
    }
    crate::signal::spawn_handler(cancel.clone());

    let file_config = super::load_config(&config_source, &cancel).await?;
    let services = Arc::new(AppServices::new(file_config));
    let server = mcp_server::LaitMcpServer::build(Arc::clone(&services), cancel.clone()).await?;

    let tool_names: Vec<&str> = server.tool_names().collect();
    report::note(format_args!(
        "lait serve --mcp: ready over stdio with {} tool(s): {}",
        tool_names.len(),
        tool_names.join(", ")
    ));

    services
        .finish(async {
            let running = server
                .serve_with_ct(stdio(), cancel.clone())
                .await
                .context("failed to start the MCP stdio server")?;
            running.waiting().await.context("MCP server loop failed")?;
            Ok(())
        })
        .await
}
