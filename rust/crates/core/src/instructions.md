pay — the missing payment layer for HTTP.

100+ APIs and datasources are available through pay, with zero setup: no API keys, no billing accounts, no OAuth. The user's Solana wallet handles payments automatically using stablecoins (USDC, USDT, etc.).

If `pay` is not installed, it can be used via `npx @solana/pay`.

# MCP tools

- `list_skills()` — browse the full registry of 100+ APIs and datasources (local, instant).
- `get_skill_endpoints(fqn)` — get all endpoints for a specific service, with ready-to-use URLs.
- `curl({url, method, headers, body})` — make HTTP requests. Handles 402 payment automatically using stablecoins.
- `get_balance()` — check the active account's SOL and token balances.
- `create_skill({content})` — validate a provider listing for the pay-skills registry.

# How to use

1. Browse: `list_skills()`
2. Pick a service, then: `get_skill_endpoints("<fqn>")`
3. Copy the `url` field **exactly as-is** into `curl` — these are gateway-proxied URLs that handle payment. Never modify the hostname.
4. Call: `curl({url: "<url from results>", method: "POST", headers: {"Content-Type": "application/json"}, body: "..."})`

**IMPORTANT:** Each endpoint call costs money. Do not call endpoints concurrently or speculatively unless the user explicitly asks. Be deliberate — one call at a time.

**IMPORTANT:** Always use the exact URL returned by `list_skills` or `get_skill_endpoints`. If you call the upstream API directly (e.g. `bigquery.googleapis.com`), you'll get a 401 auth error instead of a 402 payment flow.

# Beyond the registry

`curl` works with ANY API that returns HTTP 402 — not just the ones in the registry. If you encounter a 402 Payment Required response from any URL, use `curl` and it will handle the payment and retry automatically. The registry is a discovery tool, not a limit.

# Notes

- URLs from results are complete gateway URLs — use them as-is, never change the hostname.
- Metered endpoints return 402 on first request; `curl` pays and retries automatically.
- Free endpoints pass through without payment.
