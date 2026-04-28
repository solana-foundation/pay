//! MCP server — thin dispatch layer.
//!
//! Each tool's logic and params live in `tools/<name>.rs`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::tools;

pub struct PayMcp {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
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
        description = r#"Make an HTTP request through Pay with 402 Payment Required handling.

Use this as the primary HTTP tool for Pay gateway URLs and for any URL that
returns HTTP 402. The tool prepares MPP, x402, or SIWX credentials, asks for
local wallet approval when payment is required, then retries the original
request with the proof. The active Pay account only needs supported
stablecoins such as USDC, USDT, or CASH; it does not need SOL for network fees.
Server-side fee payers handle transaction fees and setup costs. Copy URLs
returned by `search_skills` or `get_skill_endpoints` exactly; do not replace
them with upstream API hosts.

`body` may be a string or a JSON value. JSON values are serialized before the
request and `Content-Type: application/json` is added when no content type is
provided.
"#
    )]
    async fn curl(
        &self,
        Parameters(params): Parameters<tools::curl::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::curl::run(params).await
    }

    #[tool(
        description = r#"Search paid API services for a user task and return ranked candidates with endpoint context.

Use this as the first provider-selection action for Pay-owned tasks. Pass the
user's actual task as `query`, such as "search Instagram influencers in Paris"
or "run SQL over public crypto datasets". The response is ranked and includes
reasons, endpoint/pricing candidates, tie-breaker guidance, call-plan fields,
and the next provider-selection step. Select an endpoint only when it clearly
matches the task; otherwise inspect one likely provider with
`get_skill_endpoints` or ask the user.
"#
    )]
    async fn search_skills(
        &self,
        Parameters(params): Parameters<tools::search_skills::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::search_skills::run(params).await
    }

    #[tool(description = r#"List all available paid API services.

Browse-only fallback. Do not use this for normal provider selection; call
`search_skills` with the user's task instead. Returns every service with fqn,
description, category, and use_case.
"#)]
    async fn list_skills(
        &self,
        Parameters(params): Parameters<tools::list_skills::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::list_skills::run(params).await
    }

    #[tool(
        description = r#"Get full details for a specific API service by its fqn.

Returns endpoints (each with a complete `url` for the `curl` tool),
usage notes, pricing info, sandbox/production URLs, and a next-step hint. Call
this after picking a service from `search_skills` when endpoint candidates are
not enough to make a precise paid-call plan.
"#
    )]
    async fn get_skill_endpoints(
        &self,
        Parameters(params): Parameters<tools::get_skill_endpoints::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::get_skill_endpoints::run(params).await
    }

    #[tool(description = r#"Get the balance of the active pay account.

Returns stablecoin balances for the currently configured account. Paid API
calls spend supported stablecoins such as USDC, USDT, or CASH; the account does
not need SOL for network fees because server-side fee payers handle fees and
setup costs. Use this to check available funds before making paid API calls.
"#)]
    async fn get_balance(
        &self,
        Parameters(params): Parameters<tools::get_balance::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::get_balance::run(params).await
    }

    #[tool(description = r#"Create or validate a pay-skills provider listing.

Use this when a developer wants to publish a payment-gated API in
https://github.com/solana-foundation/pay-skills. Pass the complete provider
markdown file as `content`: YAML frontmatter between `---` delimiters followed
by optional execution notes. The tool validates required metadata, endpoint
shape, URL safety, pricing precision, and paid-endpoint expectations.

Before calling, inspect real code, OpenAPI specs, deployed routes, or
`pay server start` YAML. Do not invent endpoints, prices, supported networks,
or payment protocols. If runtime YAML exists, use `pay skills provider sync`
as a starting point, then validate the generated markdown with this tool.

For detailed authoring guidance, use the Pay skill reference
`references/monetize-api.md`.
"#)]
    async fn create_skill(
        &self,
        Parameters(params): Parameters<tools::create_skill::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::create_skill::run(params).await
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServerHandler;

    #[test]
    fn server_info_has_instructions() {
        let mcp = PayMcp::new();
        let info = mcp.get_info();
        assert!(info.instructions.is_some());
        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("Tool Routing"));
        assert!(instructions.contains("search_skills({query})"));
        assert!(instructions.contains("Provider Selection Rules"));
        assert!(instructions.contains("Failure Recipes"));
        assert!(instructions.contains("402"));
    }

    #[test]
    fn server_info_protocol_version() {
        let mcp = PayMcp::new();
        let info = mcp.get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2025_06_18);
    }

    #[test]
    fn tool_descriptions_keep_provider_selection_pay_first() {
        let source = include_str!("server.rs");
        assert!(source.contains("Use this as the first provider-selection action"));
        assert!(source.contains("tie-breaker guidance"));
        assert!(source.contains("local wallet approval"));
        assert!(source.contains("does not need SOL for network fees"));
        assert!(source.contains("Server-side fee payers handle"));
        assert!(!source.contains(concat!("Bash tool", " with curl/wget")));
    }
}
