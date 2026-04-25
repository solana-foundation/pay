pay gives agents paid HTTP access without API keys. The user's wallet pays 402
requests automatically in stablecoins.

# Tools

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

`curl` also works with any non-registry URL that returns HTTP 402. Treat the
registry as discovery, not as the only supported surface.
