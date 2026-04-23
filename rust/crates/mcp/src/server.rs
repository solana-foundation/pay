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
        Parameters(params): Parameters<tools::curl::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::curl::run(params).await
    }

    #[tool(
        description = r#"Search for available paid API services and their endpoints.

Returns matching services with their endpoints. Each endpoint has a
complete `url` field — paste it directly into the `curl` tool.
For BigQuery, the project ID in URLs is `gateway-402`.

Shows top 5 metered + 3 free endpoints per service. Use
`get_skill_endpoints` for the full list if needed.

Categories: ai_ml, data, compute, maps, search, translation, productivity
"#
    )]
    async fn search_skills(
        &self,
        Parameters(params): Parameters<tools::search_skills::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::search_skills::run(params).await
    }

    #[tool(
        description = r#"List all available paid API services with their top endpoints in a single call.

Returns every service with up to 3 metered endpoints each. Use this
instead of multiple `search_skills` calls when you want a broad overview
of what's available. Drill into a specific service with `get_skill_endpoints`.
"#
    )]
    async fn list_skills(
        &self,
        Parameters(params): Parameters<tools::list_skills::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::list_skills::run(params).await
    }

    #[tool(description = r#"List all endpoints for a specific API service.

Each endpoint includes a complete `url` field — paste it directly into
the `curl` tool. No URL assembly needed. Use after `search_skills` to
get the exact endpoints you need.
"#)]
    async fn get_skill_endpoints(
        &self,
        Parameters(params): Parameters<tools::get_skill_endpoints::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::get_skill_endpoints::run(params).await
    }

    #[tool(description = r#"Get the balance of the active pay account.

Returns the SOL balance and all SPL token balances (USDC, USDT, etc.)
for the currently configured account. Use this to check available funds
before making paid API calls.
"#)]
    async fn get_balance(
        &self,
        Parameters(params): Parameters<tools::get_balance::Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        tools::get_balance::run(params).await
    }

    #[tool(description = r#"Create or validate a pay-skills provider listing.

Use this tool when a developer wants to list their API in the pay-skills
registry. It validates a provider `.md` file (YAML frontmatter + markdown body)
and returns detailed feedback.

## Workflow

1. Analyse the developer's codebase to find x402 or MPP payment-gated endpoints.
   Look for: 402 response handling, x-payment headers, pricing/metering config,
   `pay server start` YAML specs, or endpoint pricing declarations.

2. Infer the provider name from the git remote origin (e.g. `myorg/myrepo` → org is `myorg`).

3. Build the `.md` file content with YAML frontmatter containing:
   - name: API name (lowercase, matches filename)
   - title: Human-readable title
   - description: One sentence, max 120 chars
   - category: one of [ai_ml, data, compute, maps, search, translation, productivity,
     finance, identity, storage, messaging, media, iot, security, analytics, devtools, other]
   - service_url: live URL where the API is reachable
   - endpoints: array of {method, path, description, resource?, pricing?}

4. Call this tool with the full `.md` content to validate it.

5. If validation fails, the tool returns detailed errors and the JSON Schema. Fix and retry.

6. When valid, either:
   - Pass `output_path` to write the file to disk, OR
   - Tell the developer to fork https://github.com/solana-foundation/pay-skills,
     add the file at `providers/<org>/<name>.md`, and open a PR.

## Example input

```
---
name: my-api
title: "My API"
description: "Real-time fraud detection for payment transactions"
category: finance
service_url: https://api.mycompany.com
endpoints:
  - method: POST
    path: "v1/check"
    resource: "fraud"
    description: "Check a transaction for fraud signals"
    pricing:
      dimensions:
        - direction: usage
          unit: requests
          scale: 1
          tiers:
            - price_usd: 0.05
---

Longer description with usage examples, pricing notes, etc.
```
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
        assert!(info.instructions.unwrap().contains("402"));
    }

    #[test]
    fn server_info_protocol_version() {
        let mcp = PayMcp::new();
        let info = mcp.get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2025_06_18);
    }
}
