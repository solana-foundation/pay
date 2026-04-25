---
name: pay
description: Paid HTTP access for agents without API keys. Use when a task needs x402/402 APIs, pay-skills providers, wallet-paid API calls, or creating provider listings for pay-skills.
---

pay gives agents paid HTTP access without API keys. Your wallet pays 402
requests automatically in stablecoins.

# Setup

Add to your MCP config to give AI agents direct access to paid APIs:

```json
{
  "mcpServers": {
    "pay": {
      "command": "pay",
      "args": ["mcp"]
    }
  }
}
```

Or launch Claude Code / Codex with pay injected automatically:

```sh
pay claude
pay codex
```

If `pay` is not installed, use `npx @solana/pay`.

# MCP tools

- `list_skills()` - search or browse available API providers.
- `get_skill_endpoints(fqn)` - return ready-to-call endpoint URLs for one provider.
- `curl({url, method, headers, body})` - make HTTP requests and handle 402 payment.
- `get_balance()` - check wallet balances before paid work or when asked.
- `create_skill({content})` - validate a pay-skills provider listing.

# Agent workflow

1. Use `list_skills()` only when you need to choose a provider.
2. Call `get_skill_endpoints("<fqn>")` for the selected provider.
3. Copy the returned `url` exactly into `curl`; do not change the hostname.
4. Make the smallest useful request first. Paid calls should be deliberate and
   sequential unless the user explicitly asks for batching or parallel calls.

Use gateway URLs from pay results, not upstream URLs such as
`bigquery.googleapis.com`; upstream calls usually require provider-specific auth
and bypass the payment flow.

# Beyond the registry

`curl` works with any API that returns HTTP 402. The registry is discovery, not a
limit.

# CLI usage

```sh
pay setup                         # create a wallet
pay claude                        # launch Claude Code with pay
pay codex                         # launch Codex with pay
pay curl <url>                    # HTTP request with 402 handling
pay --sandbox curl <url>          # use an ephemeral devnet wallet
pay skills list                   # browse the API registry
pay skills endpoints <provider>   # list provider endpoints
pay account list                  # list accounts
pay topup                         # fund account
pay server start                  # run a payment gateway for your API
```

# Notes

- URLs from results are complete gateway URLs; use them as-is.
- Metered endpoints return 402 first; `curl` pays and retries automatically.
- Free endpoints pass through without payment.
- Use `create_skill` only when creating or reviewing a pay-skills provider file.
