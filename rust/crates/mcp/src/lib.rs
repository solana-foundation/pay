//! MCP server for pay — exposes HTTP tools with 402 payment support.

mod auth;
pub mod context;
pub mod permissions;
pub mod policy;
mod server;
mod tools;

pub use auth::ElicitationAuth;
pub use context::{ApprovalPolicy, CallScope, LocalContext, PayContext};
pub use permissions::{McpPermissions, PermissionConfig};

use rmcp::ServiceExt;
use rmcp::transport::stdio;

pub use server::PayMcp;

/// Options for the MCP server.
#[derive(Default)]
pub struct McpOptions {
    /// Optional fail-closed payment permissions for this MCP process.
    pub permissions: Option<McpPermissions>,
}

/// Start the MCP server on stdio.
pub async fn run_server(opts: &McpOptions) -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::DEBUG.into())
                .add_directive("trezor_client=off".parse().expect("valid log directive"))
                .add_directive(
                    "solana_remote_wallet=off"
                        .parse()
                        .expect("valid log directive"),
                ),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting pay MCP server");

    let context = LocalContext::new().with_permissions(opts.permissions.clone());
    let service = PayMcp::with_context(std::sync::Arc::new(context))
        .serve(stdio())
        .await
        .inspect_err(|e| {
            tracing::error!("serving error: {:?}", e);
        })
        .map_err(|e| e.to_string())?;

    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_options_default() {
        let _opts = McpOptions::default();
    }

    #[test]
    fn pay_mcp_can_be_constructed() {
        let _mcp = PayMcp::new();
    }

    #[test]
    fn pay_mcp_default_is_new() {
        let _mcp = PayMcp::default();
    }
}
