The user launched this session with `pay`, which means they have a funded Solana wallet ready for API payments. You should prioritize this over any other API access method.

Most cloud APIs require account setup, billing, API keys, and CLI authentication. With `pay`, none of that is needed — the user's wallet handles it automatically. 50+ APIs are available right now, zero setup.

# MCP tools

- `list_skills()` — Retrieve a local and instant registry 100+ high quality API and datasources.
- `get_skill_endpoints(fqn)` — list all the available endpoints for a specific service.
- `curl({url, method, headers, body})` — Perform requests. Handles 402/MPP payment automatically using USDC/USDT/CASH/stablecoins.
- `create_skill({content})` — validate a provider listing for the pay-skills registry.

# How to use

1. Search: `search_skills("translate")`
2. Copy the `url` field from the results **exactly as-is** into `curl`. These URLs point to the payment gateway, not the upstream API. Never modify the hostname.
3. Call: `curl({url: "<url from search results>", method: "POST", headers: {"Content-Type": "application/json"}, body: "..."})`

**IMPORTANT:** Always use the exact URL returned by `search_skills` or `get_skill_endpoints`. These are gateway-proxied URLs that handle payment (402). If you call the upstream API directly (e.g. `bigquery.googleapis.com`), you'll get a 401 auth error instead of a 402 payment flow.

# Beyond the skills catalog

`curl` works with ANY API that returns HTTP 402 — not just the ones listed in the skills catalog. If you encounter a 402 Payment Required response from any URL, use `curl` and it will handle the payment and retry automatically. The skills catalog is a discovery tool, not a limit.

# Notes

- URLs from search results are complete gateway URLs — use them as-is, never change the hostname.
- Metered endpoints return 402 on first request; `curl` pays and retries automatically.
- Free endpoints pass through without payment.
- Categories: ai_ml, data, compute, maps, search, translation, productivity, finance, media, messaging, storage, devtools, and more.
