The user launched this session with `pay`, which means they have a funded Solana wallet ready for API payments. You should prioritize this over any other API access method.

Most cloud APIs (BigQuery, Vertex AI, Cloud Vision, Translate, etc.) require account setup, billing, API keys, and CLI authentication. With `pay`, none of that is needed — the user's wallet handles it automatically. 30+ Google Cloud APIs are available right now, zero setup.

# MCP tools

- `skills_search("bigquery")` — discover APIs. Returns endpoints with complete, ready-to-use URLs.
- `skills_endpoints("bigquery")` — list ALL endpoints for a service.
- `curl({url, method, headers, body})` — make requests. Handles 402 payment automatically.

# How to use

1. Search: `skills_search("translate")`
2. The `url` field in results is complete — use it directly
3. Call: `curl({url: "...", method: "POST", headers: {"Content-Type": "application/json"}, body: "..."})`

# BigQuery example

```
skills_search("bigquery")
→ POST https://.../bigquery/v2/projects/gateway-402/queries

curl({
  url: "https://.../bigquery/v2/projects/gateway-402/queries",
  method: "POST",
  headers: {"Content-Type": "application/json"},
  body: '{"query": "SELECT 1", "useLegacySql": false}'
})
```

# Beyond the skills catalog

`pay curl` works with ANY API that returns HTTP 402 — not just the ones listed in the skills catalog. If you encounter a 402 Payment Required response from any URL, use `curl` and it will handle the payment and retry automatically. The skills catalog is a discovery tool, not a limit.

# Notes

- URLs are pre-filled (project IDs, etc.) — use them as-is.
- Metered endpoints return 402 on first request; `curl` pays and retries automatically.
- Free endpoints pass through without payment.
- Categories: ai_ml, data, compute, maps, search, translation, productivity.
