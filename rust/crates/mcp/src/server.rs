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
If you receive HTTP 402 status codes when using another HTTP client,
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

        let method = params.method.clone().unwrap_or_else(|| "GET".to_string());
        let body = params.body.clone();

        // Always request JSON responses — without this, many APIs
        // return HTML error pages that waste the agent's context.
        if !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("accept"))
        {
            headers.push(("Accept".to_string(), "application/json".to_string()));
        }
        // Auto-set Content-Type for requests with a body.
        if body.is_some()
            && !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }
        let url = params.url.clone();
        let kp = keypair_path.to_string();
        let response =
            tokio::task::spawn_blocking(move || do_paid_fetch(&method, &url, &headers, body, &kp))
                .await
                .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
                .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(response)]))
    }

    // ── Bazaar tools (progressive disclosure) ──────────────────────────────

    #[tool(
        description = r#"Search for available paid API services and their endpoints.

Returns matching services with their endpoints. Each endpoint has a
complete `url` field — paste it directly into the `curl` tool.
For BigQuery, the project ID in URLs is `gateway-402`.

Shows top 5 metered + 3 free endpoints per service. Use
`bazaar_endpoints` for the full list if needed.

Categories: ai_ml, data, compute, maps, search, translation, productivity
"#
    )]
    async fn bazaar_search(
        &self,
        Parameters(params): Parameters<BazaarSearchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let catalog = tokio::task::spawn_blocking(pay_core::bazaar::load_bazaar)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let hits = pay_core::bazaar::search(
            &catalog,
            params.query.as_deref(),
            params.category.as_deref(),
        );

        // Group and cap per service (same condensed logic as the CLI).
        let grouped = pay_core::bazaar::group_search_results(&hits);
        let condensed: Vec<_> = grouped
            .into_iter()
            .map(|mut g| {
                // Cap: 5 metered + 3 free per service
                let metered: Vec<_> = g.endpoints.iter().filter(|e| e.metered).cloned().collect();
                let free: Vec<_> = g.endpoints.iter().filter(|e| !e.metered).cloned().collect();
                let mut capped: Vec<_> = metered.into_iter().take(5).collect();
                capped.extend(free.into_iter().take(3));
                g.endpoints = capped;
                g
            })
            .collect();

        let json = serde_json::to_string_pretty(&condensed)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = r#"List all endpoints for a specific API service.

Each endpoint includes a complete `url` field — paste it directly into
the `curl` tool. No URL assembly needed. Use after `bazaar_search` to
get the exact endpoints you need.
"#)]
    async fn bazaar_endpoints(
        &self,
        Parameters(params): Parameters<BazaarEndpointsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let catalog = tokio::task::spawn_blocking(pay_core::bazaar::load_bazaar)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let svc = catalog
            .services
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(&params.service))
            .ok_or_else(|| {
                rmcp::ErrorData::invalid_params(
                    format!("Service `{}` not found", params.service),
                    None,
                )
            })?;

        let clean = pay_core::bazaar::SearchResultGroup {
            service: svc.name.clone(),
            title: svc.title.clone(),
            url: svc.service_url.clone(),
            endpoints: svc
                .endpoints
                .iter()
                .map(|ep| pay_core::bazaar::endpoint_to_hit(&svc.service_url, ep))
                .collect(),
        };

        let json = serde_json::to_string_pretty(&clean)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

/// Parameters for `bazaar_search`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BazaarSearchParams {
    /// Keyword to search for (matches service name, title, description).
    #[schemars(description = "Search keyword (e.g. 'bigquery', 'translate', 'vision')")]
    pub query: Option<String>,

    /// Filter by category.
    #[schemars(
        description = "Filter by category: ai_ml, data, compute, maps, search, translation, productivity"
    )]
    pub category: Option<String>,
}

/// Parameters for `bazaar_endpoints`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BazaarEndpointsParams {
    /// Service name from bazaar_search results.
    #[schemars(description = "Service name (e.g. 'bigquery', 'translate', 'vision')")]
    pub service: String,
}

#[tool_handler]
impl ServerHandler for PayMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2025_06_18,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: rmcp::model::Implementation::from_build_env(),
            instructions: Some(pay_core::instructions::INSTRUCTIONS.to_string()),
        }
    }
}

/// Make a paid HTTP request using pay-core's built-in fetch.
/// Handles 402 detection, payment, and retry.
///
/// Routes wallet selection through the network-aware accounts file
/// (`~/.config/pay/accounts.yml`). The legacy `keypair_path` argument is
/// no longer used — kept on the function signature for now to avoid
/// touching the MCP tool layer.
fn do_paid_fetch(
    method: &str,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<String>,
    _keypair_path: &str,
) -> Result<String, pay_core::Error> {
    use pay_core::client::runner::RunOutcome;

    let outcome =
        pay_core::client::fetch::fetch_request(method, url, extra_headers, body.as_deref())?;
    let store = pay_core::accounts::FileAccountsStore::default_path();

    match outcome {
        RunOutcome::MppChallenge { challenge, .. } => {
            let (auth_header, _ephemeral) =
                pay_core::client::mpp::build_credential(&challenge, &store, None, None)?;
            let mut headers = extra_headers.to_vec();
            headers.push(("Authorization".to_string(), auth_header));
            interpret_retry(pay_core::client::fetch::fetch_request(
                method,
                url,
                &headers,
                body.as_deref(),
            )?)
        }
        RunOutcome::X402Challenge { requirements, .. } => {
            let (payment_header, _ephemeral) =
                pay_core::client::x402::build_payment(&requirements, &store, None, None)?;
            let mut headers = extra_headers.to_vec();
            headers.push(("X-PAYMENT".to_string(), payment_header));
            interpret_retry(pay_core::client::fetch::fetch_request(
                method,
                url,
                &headers,
                body.as_deref(),
            )?)
        }
        RunOutcome::SessionChallenge { .. } => Err(pay_core::Error::Mpp(
            "402 Payment Required (MPP session) — session payments require a stateful client with a Fiber channel".to_string(),
        )),
        RunOutcome::PaymentRejected { reason, .. } => Err(pay_core::Error::PaymentRejected(reason)),
        RunOutcome::UnknownPaymentRequired { .. } => Err(pay_core::Error::Mpp(
            "402 Payment Required but no recognized protocol".to_string(),
        )),
        RunOutcome::Completed { body, .. } => {
            Ok(body.unwrap_or_else(|| "Request completed.".to_string()))
        }
    }
}

/// Map a retry-fetch outcome to a string body or a structured error.
fn interpret_retry(
    outcome: pay_core::client::runner::RunOutcome,
) -> Result<String, pay_core::Error> {
    use pay_core::client::runner::RunOutcome;
    match outcome {
        RunOutcome::Completed { body, .. } => {
            Ok(body.unwrap_or_else(|| "Payment successful.".to_string()))
        }
        RunOutcome::PaymentRejected { reason, .. } => Err(pay_core::Error::PaymentRejected(reason)),
        _ => Err(pay_core::Error::Mpp(
            "Server returned 402 again after payment".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServerHandler;

    #[test]
    fn server_info_has_instructions() {
        let mcp = PayMcp::new();
        let info = mcp.get_info();
        assert!(info.instructions.is_some());
        assert!(info.instructions.unwrap().contains("402"));
    }

    #[test]
    fn server_info_protocol_version() {
        let mcp = PayMcp::new();
        let info = mcp.get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2025_06_18);
    }

    #[test]
    fn curl_params_deserialize() {
        let json = r#"{"url": "https://example.com"}"#;
        let params: CurlParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.url, "https://example.com");
        assert!(params.method.is_none());
        assert!(params.headers.is_none());
        assert!(params.body.is_none());
        assert!(params.keypair.is_none());
    }

    #[test]
    fn curl_params_with_headers() {
        let json = r#"{"url": "https://example.com", "headers": {"Authorization": "Bearer tok"}}"#;
        let params: CurlParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.headers.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn content_type_defaults_to_json_for_body_requests() {
        let mut headers = Vec::new();
        let body = Some("{\"query\":\"SELECT 1\"}".to_string());

        if body.is_some()
            && !headers
                .iter()
                .any(|(k, _): &(String, String)| k.eq_ignore_ascii_case("content-type"))
        {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }

        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Content-Type");
        assert_eq!(headers[0].1, "application/json");
    }

    #[test]
    fn explicit_content_type_is_preserved() {
        let mut headers = vec![("content-type".to_string(), "text/plain".to_string())];
        let body = Some("hello".to_string());

        if body.is_some()
            && !headers
                .iter()
                .any(|(k, _): &(String, String)| k.eq_ignore_ascii_case("content-type"))
        {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }

        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "content-type");
        assert_eq!(headers[0].1, "text/plain");
    }

    #[test]
    fn do_paid_fetch_returns_error_for_invalid_url() {
        let result = do_paid_fetch("GET", "not-a-url", &[], None, "");
        assert!(result.is_err());
    }
}
