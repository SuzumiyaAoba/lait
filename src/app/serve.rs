//! `lait serve --mcp`: CLI-facing orchestration — argument validation,
//! config loading, transport selection (stdio, or Streamable HTTP with
//! `--http`), and handing off to `mcp_server::LaitMcpServer` (the actual
//! MCP protocol/tool-catalog implementation). See docs/usage/ja/serve.md.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rmcp::{
    ServiceExt,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService, stdio,
        streamable_http_server::session::local::LocalSessionManager,
    },
};

use crate::{chat, cli::ServeArgs, config::ConfigSource, engine::AppServices, mcp_server, report};

pub(super) async fn run(
    args: ServeArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    if !args.mcp {
        bail!("'lait serve' currently only supports '--mcp'; see docs/usage/ja/serve.md");
    }
    crate::signal::spawn_handler(cancel.clone());

    let file_config = super::load_config(&config_source, &cancel).await?;
    let (cache_enabled, cache_ttl) = chat::resolve_cache_settings(cache_override, &file_config);
    let options = mcp_server::ServeOptions {
        cache_enabled,
        cache_ttl,
        record_dir: args.record,
        replay_dir: args.replay,
        trace_file: args.trace_file,
    };
    let services = Arc::new(AppServices::new(file_config));
    let server =
        mcp_server::LaitMcpServer::build(Arc::clone(&services), options, cancel.clone()).await?;

    let tool_names: Vec<&str> = server.tool_names().collect();
    let tool_summary = format!("{} tool(s): {}", tool_names.len(), tool_names.join(", "));

    services
        .finish(async {
            match args.http {
                None => {
                    report::note(format_args!(
                        "lait serve --mcp: ready over stdio with {tool_summary}"
                    ));
                    let running = server
                        .serve_with_ct(stdio(), cancel.clone())
                        .await
                        .context("failed to start the MCP stdio server")?;
                    running.waiting().await.context("MCP server loop failed")?;
                    Ok(())
                }
                Some(address) => {
                    serve_http(server, address, args.allowed_host, &tool_summary, cancel).await
                }
            }
        })
        .await
}

/// Serves `server` over MCP's Streamable HTTP transport on `address` until
/// `cancel` fires. Each accepted connection is served on its own task over
/// HTTP/1; rmcp's `StreamableHttpService` (a plain `tower` service) owns
/// the protocol, one `LaitMcpServer` clone per client session. It ignores
/// the request path, so `http://ADDR/mcp` (the conventional one, and what
/// the startup note prints) and any other path reach the same endpoint.
async fn serve_http(
    server: mcp_server::LaitMcpServer,
    address: std::net::SocketAddr,
    allowed_hosts: Vec<String>,
    tool_summary: &str,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to listen on {address}"))?;
    let local_address = listener
        .local_addr()
        .context("failed to read the listening address")?;

    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = cancel.child_token();
    config.allowed_hosts.extend(allowed_hosts);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    report::note(format_args!(
        "lait serve --mcp: ready at http://{local_address}/mcp with {tool_summary}"
    ));

    loop {
        let stream = tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::debug!(error = %error, "failed to accept an MCP HTTP connection");
                    continue;
                }
            },
        };
        let service = hyper_util::service::TowerToHyperService::new(service.clone());
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let connection = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service);
            tokio::select! {
                result = connection => {
                    if let Err(error) = result {
                        tracing::debug!(error = %error, "MCP HTTP connection ended with an error");
                    }
                }
                () = cancel.cancelled() => {}
            }
        });
    }
    Ok(())
}
