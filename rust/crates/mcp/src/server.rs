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

    #[tool(description = r#"List all available paid API services.

Returns every service with fqn, description, category, and use_case.
Browse the list and pick the best match for your task, then call
`get_skill_endpoints` with the chosen `fqn` to get endpoints and
usage instructions.
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
usage notes, pricing info, and sandbox/production URLs. Call this
after picking a service from `search_skills` or `list_skills`.
"#
    )]
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

Use this when a developer wants to publish a payment-gated API in
https://github.com/solana-foundation/pay-skills. The tool validates a complete
provider `.md` file: YAML frontmatter followed by optional markdown body.

## What to inspect before calling

1. Find real payment-gated endpoints. Look for HTTP 402 handling, x402 headers,
   MPP challenges, `pay server start` YAML, pricing config, or metering config.
2. Prefer facts from code, OpenAPI specs, existing routing tables, and deployed
   URLs. Do not invent endpoints or prices.
3. If the project has runtime `pay server start` YAML, consider using
   `pay skills provider sync` first, then validate the generated `.md` content
   with this tool.

## Required provider rules

- File path: `providers/<operator>/<name>.md` for native APIs or
  `providers/<operator>/<origin>/<name>.md` for proxied APIs.
- `name`: lowercase URL-safe API name; must match the filename without `.md`.
- `title`: human-readable API name.
- `description`: 64-255 chars; summarize what the API is, major capabilities,
  and result shapes. Do not start with "Use for".
- `use_case`: 32-255 chars; start with "Use for" or "Use when" and list
  concrete agent trigger tasks, synonyms, and adjacent workflows.
- `category`: one of `ai_ml`, `analytics`, `cloud`, `compute`, `data`,
  `devtools`, `finance`, `identity`, `iot`, `maps`, `media`, `messaging`,
  `other`, `productivity`, `search`, `security`, `storage`, `translation`.
- `service_url`: production HTTPS URL with a domain name, not localhost or an IP.
- `endpoints`: at least one `{method, path, description, resource?, pricing?}`.
- Endpoint descriptions: 32-255 chars, start with a verb and name the object.

## Root metadata guidance

- Use the 255-character budget deliberately; 180-255 chars is a good target for
  broad APIs.
- `description` is catalog/search copy: what the service does and what callers
  get back.
- `use_case` is routing copy: when an agent should pick the service for a user
  task.
- Keep both truthful to the listed endpoints. Do not include unsupported
  workflows just to attract traffic.

## Pricing and probing rules

- Omit `pricing` for free endpoints.
- If `pricing` is present, CI treats the endpoint as paid and probes for HTTP 402.
- Paid endpoints must return a valid MPP, MPP session, or x402 challenge.
- Paid endpoints must accept Solana mainnet USDC or USDT.
- Non-zero per-unit prices must satisfy `price_usd / scale >= 0.000001` because
  USDC and USDT have 6 decimal places.

## Workflow

1. Build the full markdown content.
2. Call this tool with `content`.
3. If validation fails, fix every reported issue and retry.
4. When valid, either pass `output_path` or tell the developer to add the file
   to pay-skills and run:

```bash
pay skills build . --output /tmp/pay-skills-dist
pay skills probe . --files providers/<operator>/<name>.md --currencies USDC,USDT --timeout 15 --concurrency 5
```

## Example input

```
---
name: fraud-check
title: "Fraud Check"
description: "Score payment transactions for fraud risk using device, behavioral, merchant, card, wallet, and checkout signals. Returns risk scores, review signals, decision reasons, and structured metadata for payment protection workflows."
use_case: "Use for payment fraud scoring, checkout risk review, transaction monitoring, merchant risk signals, chargeback prevention, wallet or card payment review, suspicious behavior detection, and enriching trust and safety workflows."
category: finance
service_url: https://api.mycompany.com
endpoints:
  - method: POST
    path: v1/check
    resource: fraud
    description: "Score a payment transaction for fraud risk signals"
    pricing:
      dimensions:
        - direction: usage
          unit: requests
          scale: 1
          tiers:
            - price_usd: 0.05
---

Use this API to score card, wallet, and bank payment attempts before fulfillment.
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
