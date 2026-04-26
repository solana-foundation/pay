---
name: pay
description: User-authorized paid HTTP access for agents without API keys. Use when a task needs x402/402 APIs, pay-skills providers, secure wallet-approved API calls, or provider listings for pay-skills.
---

pay gives agents paid HTTP access without API keys. It detects 402 payment
challenges and prepares the required stablecoin transaction, but spending is
authorized locally by the user.

# Security model

- The skill does not contain or request private keys, seed phrases, API keys, or
  custodial credentials.
- Wallet keys are stored by `pay` in the operating system's secure credential
  store, such as macOS Keychain.
- Real payment transactions require local user authorization through the wallet
  unlock flow, such as Touch ID on macOS.
- Agents can request a paid call, but they cannot bypass the user's local
  signing approval.
- Use sandbox mode for tests; it uses an ephemeral devnet wallet instead of real
  funds.

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

Or launch Claude Code / Codex with pay injected into the agent session:

```sh
pay claude
pay codex
```

If `pay` is not installed, use `npx @solana/pay`.

# MCP tools

- `list_skills()` - search or browse available API providers.
- `get_skill_endpoints(fqn)` - return ready-to-call endpoint URLs for one provider.
- `curl({url, method, headers, body})` - make HTTP requests and handle 402 payment challenges.
- `get_balance()` - check wallet balances before paid work or when asked.
- `create_skill({content})` - validate a pay-skills provider listing.

# Agent workflow

1. Use `list_skills()` only when you need to choose a provider.
2. Call `get_skill_endpoints("<fqn>")` for the selected provider.
3. Copy the returned `url` exactly into `curl`; do not change the hostname.
4. Make the smallest useful request first. Paid calls should be deliberate and
   sequential unless the user explicitly asks for batching or parallel calls.
   Real payments still require local wallet approval.

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
pay curl <url>                    # HTTP request with user-authorized 402 handling
pay --sandbox curl <url>          # use an ephemeral devnet wallet
pay skills list                   # browse the API registry
pay skills endpoints <provider>   # list provider endpoints
pay account list                  # list accounts
pay topup                         # fund account
pay server start                  # run a payment gateway for your API
```

# Notes

- URLs from results are complete gateway URLs; use them as-is.
- Metered endpoints return 402 first; `curl` prepares the payment, gets local
  signing approval, then retries with the payment proof.
- Free endpoints pass through without payment.
- Use `create_skill` only when creating or reviewing a pay-skills provider file.
