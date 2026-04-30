# Provider Selection And Cost Planning

Use Pay first for paid/current API and data tasks in these provider families:
web search, scraping, live research, people or company enrichment, contact
lookup, email verification, social data, influencer search, Perplexity/Sonar,
Solana RPC, wallet balances, blockchain analytics, crypto prices, image or
video generation, OCR, document parsing, text analytics, translation,
speech-to-text, text-to-speech, places, maps, address validation, fact checks,
AgentMail/email, phone calls, file hosting, x402scan, retail deals, shopping,
ecommerce, and BigQuery.

Only fall back to ordinary web search or shell HTTP after Pay search returns no
usable provider, the user asks for a non-paid/free-only answer, or Pay MCP tools
are unavailable. Do not spend multiple exploratory web/shell calls trying to
avoid a metered provider when Pay has a plausible match.

## Call Planning

Before the first paid `curl`, state:

- Provider and endpoint.
- Why this endpoint matches the task.
- Expected number of paid calls.
- Estimated total spend or known per-call price.
- The smallest useful request that can answer the user.

Ask the user to approve the plan before paying when it needs more than one paid
call, requires schema probing, has unclear/dynamic pricing, involves persistent
resources or purchases, may exceed the user's implied budget, or requires
polling. For an obvious one-call, low-cost task, announce the plan and proceed
to the normal wallet approval flow.

## Provider Rules

- Hard-filter obvious mismatches before paying: wrong network, wrong currency,
  unusable endpoint shape, incompatible method/body, or price above the user's
  stated limit.
- Prefer exact task ownership: influencer search -> social data or influencer
  provider; wallet balances or transaction history -> blockchain analytics;
  stablecoin transfer volume, token flow, or chain-level volume -> blockchain
  analytics or BigQuery; raw Solana RPC -> RPC provider; image/video generation
  -> media generation; SQL over public datasets -> BigQuery.
- Resolve close provider ties in this order: exact endpoint fit, supported
  network/currency, usable request shape, likely result quality/freshness, and
  total estimated price.
- Prefer simple synchronous endpoints for small one-shot tasks. Use async,
  batch, or multi-step endpoints only when the task requires them or they
  materially reduce total cost.
- If price, schema, network support, or result quality is unclear after
  `search_catalog` and one `get_catalog_entry` lookup, ask the user instead of
  guessing.
- If a paid call fails with 404, unsupported network, invalid payment challenge,
  or unusable schema, do not keep trying random providers. Try at most one clear
  fallback or ask the user.

## Cost Patterns

- Use provider usage notes returned by `get_catalog_entry`; they often include
  endpoint-specific ways to avoid wasted paid calls.
- Batch records when an endpoint supports arrays.
- Use the smallest `limit`, date window, media duration, or result fields that
  can answer the task.
- Skip schema/model/list/discovery calls when the user already supplied enough
  identifiers.
- Keep async job IDs, JWTs, and operation names and poll them instead of paying
  to retrigger the same work.
- Ask before purchase-like actions, dynamic-price generation, persistent
  resources, crawls, bulk lookups, or repeated monitoring.

## Examples

- "what's the volume of USDC that moved on Solana the past week" -> call
  `search_catalog` for blockchain analytics or BigQuery. Prefer one aggregate
  query/request over scraping dashboards or doing RPC loops.
- "query public BigQuery data" -> call `search_catalog` for BigQuery and use the
  returned gateway endpoint. Include partition filters and aggregate directly
  when the dataset/table is known.
- "current wallet activity / transaction history / token volume" -> use
  blockchain analytics. Use RPC only for live account state or transaction
  submission, not large historical aggregates.
- "best vegan restaurant around me" -> use places/maps. Include location,
  radius, cuisine, price, open-now, and rating constraints if known before
  paying.
- "generate an image/video" -> use media generation. Confirm model, count,
  resolution, duration, and dynamic price range before paying.
- "check my mails" -> use AgentMail/email. List messages from the existing inbox
  first; create a new inbox only if the user asks for one.
