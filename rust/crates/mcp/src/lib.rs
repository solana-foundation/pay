//! MCP server for pay — exposes curl and wget as tools with 402 payment support.

mod server;

use rmcp::ServiceExt;
use rmcp::transport::stdio;

pub use server::PayMcp;

/// Options for the MCP server.
#[derive(Default)]
pub struct McpOptions {}

/// Start the MCP server on stdio.
pub async fn run_server(_opts: &McpOptions) -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::DEBUG.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting pay MCP server");

    let service = PayMcp::new()
        .serve(stdio())
        .await
        .inspect_err(|e| {
            tracing::error!("serving error: {:?}", e);
        })
        .map_err(|e| e.to_string())?;

    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}
