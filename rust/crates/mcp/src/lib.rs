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
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, DuplexStream};

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

    let (server_input, feeder) = tokio::io::duplex(64 * 1024);
    let stdin_forwarder = tokio::spawn(forward_compatible_stdio(tokio::io::stdin(), feeder));
    let context = LocalContext::new().with_permissions(opts.permissions.clone());
    let service = PayMcp::with_context(std::sync::Arc::new(context))
        .serve((server_input, tokio::io::stdout()))
        .await
        .inspect_err(|e| {
            tracing::error!("serving error: {:?}", e);
        })
        .map_err(|e| e.to_string());

    let result = match service {
        Ok(service) => service
            .waiting()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Err(error) => Err(error),
    };
    stdin_forwarder.abort();
    let _ = stdin_forwarder.await;
    result
}

/// Goose versions that probe MCP 2026-07-28 currently send an empty
/// `server/discover` params object. The released protocol requires request
/// metadata, and rmcp intentionally rejects the malformed probe before it can
/// fall back to legacy `initialize`. Supply only the missing discovery
/// metadata; all other JSON-RPC messages pass through byte-for-byte.
async fn forward_compatible_stdio(
    input: impl AsyncRead + Unpin,
    mut output: DuplexStream,
) -> std::io::Result<()> {
    let mut input = BufReader::new(input);
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            break;
        }
        let normalized = normalize_discovery_probe(&line);
        output.write_all(normalized.as_bytes()).await?;
    }
    output.shutdown().await
}

fn normalize_discovery_probe(line: &str) -> String {
    let Ok(mut message) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    if message.get("method").and_then(serde_json::Value::as_str) != Some("server/discover") {
        return line.to_string();
    }
    let Some(params) = message
        .get_mut("params")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return line.to_string();
    };
    if params.contains_key("_meta") {
        return line.to_string();
    }
    params.insert(
        "_meta".to_string(),
        serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {
                "name": "discovery-compat-client",
                "version": "unknown"
            },
            "io.modelcontextprotocol/clientCapabilities": {}
        }),
    );
    format!("{message}\n")
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

    #[test]
    fn goose_discovery_probe_gets_required_protocol_metadata() {
        let input = r#"{"jsonrpc":"2.0","id":0,"method":"server/discover","params":{}}"#;
        let normalized: serde_json::Value =
            serde_json::from_str(&normalize_discovery_probe(input)).unwrap();
        let metadata = &normalized["params"]["_meta"];

        assert_eq!(
            metadata["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert!(metadata["io.modelcontextprotocol/clientCapabilities"].is_object());
    }

    #[test]
    fn valid_discovery_and_other_messages_are_unchanged() {
        let discovery = r#"{"jsonrpc":"2.0","id":0,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
        let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;

        assert_eq!(normalize_discovery_probe(discovery), discovery);
        assert_eq!(normalize_discovery_probe(initialize), initialize);
    }
}
