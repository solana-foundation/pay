//! MCP tool definitions for pay.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// MCP server exposing pay's HTTP tools with 402 payment support.
pub struct PayMcp {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

/// Parameters for the `curl` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CurlParams {
    /// The URL to request.
    #[schemars(description = "The URL to fetch (e.g. https://api.example.com/data)")]
    pub url: String,

    /// HTTP method (GET, POST, PUT, DELETE, etc.). Defaults to GET.
    #[schemars(description = "HTTP method. Defaults to GET.")]
    #[allow(dead_code)]
    pub method: Option<String>,

    /// Request headers as key-value pairs.
    #[schemars(
        description = "Request headers as key-value pairs (e.g. {\"Authorization\": \"Bearer token\"})"
    )]
    pub headers: Option<std::collections::HashMap<String, String>>,

    /// Request body (for POST/PUT).
    #[schemars(description = "Request body string (for POST, PUT, etc.)")]
    #[allow(dead_code)]
    pub body: Option<String>,

    /// Path to a Solana keypair file for paying 402 challenges.
    /// Defaults to ~/.config/solana/id.json.
    #[schemars(description = "Path to Solana keypair file. Defaults to ~/.config/solana/id.json.")]
    pub keypair: Option<String>,
}

/// Parameters for the `wget` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WgetParams {
    /// The URL to download.
    #[schemars(description = "The URL to download")]
    pub url: String,

    /// Output file path. Defaults to the filename from the URL.
    #[schemars(description = "Output file path. Defaults to filename from URL.")]
    #[allow(dead_code)]
    pub output: Option<String>,

    /// Path to a Solana keypair file for paying 402 challenges.
    #[schemars(description = "Path to Solana keypair file. Defaults to ~/.config/solana/id.json.")]
    pub keypair: Option<String>,
}

impl Default for PayMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl PayMcp {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = r#"Make an HTTP request with automatic 402 Payment Required handling.

If you receive HTTP 402 status codes when using the Bash tool with curl/wget,
retry the exact same request using this tool instead — it will handle the
payment automatically.

Example: if `curl https://api.example.com/data` returns 402, call this tool
with url="https://api.example.com/data" and the payment + retry is handled
for you.

Supports both MPP (www-authenticate header) and x402 (X-PAYMENT-REQUIRED) protocols.
Payments are made on Solana (SOL and SPL tokens like USDC).
"#
    )]
    async fn curl(
        &self,
        Parameters(params): Parameters<CurlParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let config = pay_core::Config::load()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let keypair_path = params
            .keypair
            .clone()
            .or_else(|| config.default_keypair_source())
            .unwrap_or_default()
            .to_string();

        // Build the response using pay-core's fetch (built-in HTTP client)
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(h) = &params.headers {
            for (k, v) in h {
                headers.push((k.clone(), v.clone()));
            }
        }

        let url = params.url.clone();
        let kp = keypair_path.to_string();
        let response = tokio::task::spawn_blocking(move || do_paid_fetch(&url, &headers, &kp))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(response)]))
    }

    #[tool(
        description = r#"Download a resource with automatic 402 Payment Required handling.

If you receive HTTP 402 status codes when using the Bash tool with wget,
retry the exact same request using this tool instead — it will handle the
payment automatically.

Example: if `wget https://api.example.com/file` returns 402, call this tool
with url="https://api.example.com/file" and the payment + download is handled
for you.

Supports both MPP (www-authenticate header) and x402 (X-PAYMENT-REQUIRED) protocols.
Payments are made on Solana (SOL and SPL tokens like USDC).
"#
    )]
    async fn wget(
        &self,
        Parameters(params): Parameters<WgetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let config = pay_core::Config::load()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let keypair_path = params
            .keypair
            .clone()
            .or_else(|| config.default_keypair_source())
            .unwrap_or_default()
            .to_string();

        let url = params.url.clone();
        let kp = keypair_path.to_string();
        let response = tokio::task::spawn_blocking(move || do_paid_fetch(&url, &[], &kp))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(response)]))
    }
}

#[tool_handler]
impl ServerHandler for PayMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2025_06_18,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: rmcp::model::Implementation::from_build_env(),
            instructions: Some(
                "Pay MCP server — HTTP tools with automatic Solana payment for 402-gated APIs.\n\
                 When you encounter HTTP 402 Payment Required responses from curl or wget in the \
                 Bash tool, retry the same request using the pay `curl` or `wget` tools instead. \
                 They will automatically detect the payment protocol (MPP or x402), sign a Solana \
                 transaction, and retry with the payment credential."
                    .to_string(),
            ),
        }
    }
}

/// Make a paid HTTP request using pay-core's built-in fetch.
/// Handles 402 detection, payment, and retry.
fn do_paid_fetch(
    url: &str,
    extra_headers: &[(String, String)],
    keypair_path: &str,
) -> Result<String, pay_core::Error> {
    use pay_core::runner::RunOutcome;

    let outcome = pay_core::fetch::fetch(url, extra_headers)?;

    match outcome {
        RunOutcome::MppChallenge { challenge, .. } => {
            let auth_header = pay_core::mpp::build_credential(&challenge, keypair_path)?;
            let mut headers = extra_headers.to_vec();
            headers.push(("Authorization".to_string(), auth_header));
            match pay_core::fetch::fetch(url, &headers)? {
                RunOutcome::Completed { body, .. } => {
                    Ok(body.unwrap_or_else(|| "Payment successful.".to_string()))
                }
                _ => Err(pay_core::Error::Mpp(
                    "Server returned 402 again after payment".to_string(),
                )),
            }
        }
        RunOutcome::X402Challenge { requirements, .. } => {
            let payment_header = pay_core::x402::build_payment(&requirements, keypair_path)?;
            let mut headers = extra_headers.to_vec();
            headers.push(("X-PAYMENT".to_string(), payment_header));
            match pay_core::fetch::fetch(url, &headers)? {
                RunOutcome::Completed { body, .. } => {
                    Ok(body.unwrap_or_else(|| "Payment successful.".to_string()))
                }
                _ => Err(pay_core::Error::Mpp(
                    "Server returned 402 again after payment".to_string(),
                )),
            }
        }
        RunOutcome::UnknownPaymentRequired { .. } => Err(pay_core::Error::Mpp(
            "402 Payment Required but no recognized protocol".to_string(),
        )),
        RunOutcome::Completed { body, .. } => {
            Ok(body.unwrap_or_else(|| "Request completed.".to_string()))
        }
    }
}
