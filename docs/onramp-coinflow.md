# Buying the first stablecoins: replacing MoonPay with Coinflow

Status: F1 built on `feat/pay-cloud`, 2026-09-16. Sandbox probed with the
Solana Foundation merchant key (`.env` at the repo root, gitignored). Ludo is
working with Coinflow support on the merchant settings below.

Scope: the funding step that follows account creation, for every backend
(platform keystore, Ledger, remote wallet). The user has a Solana address
and no stablecoins; they should be able to pay with a card and see USDC
land at that address without leaving pay's own surfaces.

## What is built (F1)

- pay-cloud `funding` module (`coinflow` cargo feature, on by default):
  `POST /api/fund/start`, `POST /api/fund/webhook`, `GET /api/fund/{id}`,
  configured by `COINFLOW_API_KEY`, `COINFLOW_ENV`, `COINFLOW_MERCHANT_ID`,
  `COINFLOW_WEBHOOK_KEY`, `COINFLOW_SETTLE_TO_CUSTOMER`. Without the key the
  endpoints answer 503 and the server says so at startup. The checkout link
  is minted server-side with amount, USDC settlement, card rails, our
  `webhookInfo` and a 30-minute expiry fixed; `destination` is added when
  customer settlement is on. Tested against a mock Coinflow and live against
  the sandbox.
- `/fund` page in the web-ui cloud app: amount presets, the exact fee table
  from Coinflow's quote, Coinflow's hosted checkout in an iframe (origin
  pinned to the environment), then a progress log that waits up to 20 s for
  the webhook-reported signature before returning to the CLI callback with
  `payment_id` and `signature`.
- CLI: `PAY_ONRAMP=coinflow` switches "Buy stablecoins" in `pay topup` and
  `pay setup` from the MoonPay redirect to the funding page, opened through
  the same loopback listener as onboarding (`Expect::Payment`). The TUI
  shows "Card charged" when the page returns and attaches the signature to
  the balance detection. `pay setup --backend cloud` now runs the funding
  step after the wallet is registered, like every other backend. MoonPay
  stays the default until the Coinflow path is verified with real
  settlement.

## Merchant settings to request from Coinflow

1. Enable third-party USDC settlement (`destination` / `destinationAuthKey`)
   on `solana-foundation` in sandbox and production.
2. Set the fee mode to merchant for card, chargeback protection, FX, gas and
   network fees, so the customer pays exactly the subtotal. Confirm that
   with a third-party destination the fee is taken from pay's Coinflow
   Wallet or invoiced, and the destination still receives the full amount.
   pay-cloud can then subsidize only the first top-up by adding a
   `customPayInFees` line for later purchases, locked in the checkout token.
3. A webhook endpoint (`https://cloud.pay.sh/api/fund/webhook`) with an
   `Authorization` value we generate, mirrored into `COINFLOW_WEBHOOK_KEY`.
4. Direct card entry on our own pages (the SDK's `CoinflowCardNumberInput`
   and `CoinflowCvvInput`, TokenEx fields) needs the page origins on the
   merchant's referrer allowlist: `https://pay.sh`, `https://cloud.pay.sh`
   and `http://localhost:3000` for development. Verified 2026-09-18: the
   sandbox merchant answers `Referrer … not allowed for merchant
   solana-foundation` (HTTP 401) on `POST /api/tokenize/iframe/config` for
   every origin but `sandbox.coinflow.cash`, so the fields never load.
   Coinflow's PCI page says this path also needs SAQ A-EP. Until then pages
   embed the hosted checkout for cards (`COINFLOW_CARD_ENTRY=hosted`, the
   default); `direct` switches to our own fields.

## What we have today

| Surface | Flow | Friction |
| --- | --- | --- |
| `pay setup` / `pay topup` TUI | "Buy stablecoins" opens the browser at pay-api `GET /v1/onramp/start`, which redirects to `buy.moonpay.com` with `walletAddress` prefilled; the TUI polls balances through pay-api until USDC shows up | Full hop to MoonPay: MoonPay account, MoonPay KYC, MoonPay UI. Completion is inferred from the balance, never confirmed. |
| MCP `topup` tool | Returns a Solana Pay QR, or a bare provider URL (Coinbase, PayPal, Venmo) | The agent can only say "go buy USDC somewhere and send it here". |
| pay-cloud onboarding page | No funding step. The provider callback returns to the terminal straight after the wallet is created. | The plan's "fund" page was going to reuse the MoonPay redirect. |

pay-api carries `MoonpayConfig`, the redirect builder, a static completion
page and onramp metrics.

## What Coinflow is

Coinflow is a merchant checkout that settles in USDC. In their model pay is
the **merchant** and the user is the **customer**. That fits the goal
exactly, with one entitlement to obtain (below).

Facts verified against the sandbox on 2026-09-16, merchant id
`solana-foundation`, base `https://api-sandbox.coinflow.cash`:

- **Card, Apple Pay and Google Pay are on** for the merchant; ACH, wire,
  Cash App, PayPal and Venmo exist as processors (all `mock` in sandbox).
  Card entry, tokenization, 3DS challenges and chargeback screening happen
  inside Coinflow's PCI iframe. No customer account, no customer KYC for
  card purchases in the docs (KYC is for payouts).
- **Fees are charged to the customer** (`creditCardFeeMode: user`, same for
  chargeback protection, FX, gas, network). Quote for a $20 purchase:

  | Line | Amount |
  | --- | --- |
  | Subtotal (USDC received) | $20.00 |
  | Card fee | $1.11 |
  | Chargeback protection | $0.59 |
  | Total charged | $21.70 |

  `POST /api/checkout/totals/{merchantId}` returns this before card entry,
  so the page can show the exact total.
- **Three integration surfaces**, all driven from a server that holds the
  API key: a hosted checkout link (`POST /api/checkout/link`, redirect or
  iframe), the React SDK `@coinflowlabs/react` (`CoinflowPurchase`), or raw
  API with our own card form (PCI scope, not for us). Verified: the link
  accepts a `standaloneLinkConfig.callbackUrl` and redirects there when the
  purchase completes (our loopback URL was accepted), a `theme` object, and
  `expiresIn`. `POST /api/checkout/jwt-token` mints a single-use token that
  locks subtotal, destination, settlement type, payment methods and
  `webhookInfo` server-side so the browser cannot change them.
- **Settlement destination is the crux.** Default is the merchant's
  Coinflow Wallet. Per-transaction settlement to an arbitrary Solana
  address exists (`destination` or `destinationAuthKey` on the link, JWT and
  SDK; `POST /api/checkout/destination-auth-key` tokenizes an address), and
  it is what an onramp needs. On our sandbox merchant it is off:

  ```
  400 {"details":"destinationAuthKey is disabled for this merchant"}
  ```

  Coinflow enables this per merchant. Until they do, every purchase settles
  to pay's merchant balance, not the user's wallet.
- **Webhooks** carry `Settled` and `Disbursed Funds` events with the
  on-chain `signature`, the destination `wallet`, amounts, and our
  `webhookInfo` passthrough. Requests are authenticated by an
  `Authorization` value we set in the dashboard. Retries for ~18 hours,
  duplicates possible.
- **Sandbox** uses mock processors and test cards (`4242 4242 4242 4242`,
  any future expiry, zip `99999` forces a decline; amounts ending in 98/97/96
  drive 3DS outcomes). Whether sandbox disbursement reaches devnet, and with
  which mint, is not documented; to be tested once destination settlement is
  enabled.

## Proposal: one funding page in pay-cloud, used by both doors

The web onboarding flow and the TUI already share one server (pay-cloud)
and one loopback pattern (the `gh auth login` style callback). The funding
step joins them: pay-cloud owns the Coinflow key, renders one terminal-styled
funding page, and reports completion to whoever opened it.

```text
 pay setup / pay topup ──open──▶ cloud.pay.sh/fund?address&callback&state ──▶ CoinflowPurchase (PCI iframe)
        ▲                                        │                                     │
        │   callback ?state&payment_id&signature  │ POST /api/fund/start (session key,  │ onSuccess(paymentId)
        └────────────────────────────────────────┤  jwt-token: destination=address)     ▼
                                                 │ ◀── Coinflow webhook: Disbursed Funds {signature, wallet}
 onboarding page ── "Wallet created" ──▶ /fund … ─┘ (same page, then "Return to your terminal")
```

### pay-cloud: `funding` module behind a `coinflow` cargo feature

- `POST /api/fund/start { address, cents, callback?, state? }`: validates the
  address and amount (presets 10/20/50 USD, min $2), mints a session key with
  `x-coinflow-auth-user-id = address`, quotes totals, and mints a checkout
  JWT locking `subtotal`, `blockchain: solana`, `settlementType: USDC`,
  `destination: address` (or a `destinationAuthKey`), `allowedPaymentMethods`,
  `webhookInfo { address, state }`, `expiresIn`. Returns `{ sessionKey,
  checkoutJwtToken, totals, env, merchantId }` to the page.
- `POST /api/fund/webhook`: verifies the dashboard `Authorization` value,
  dedupes by event id, records `paymentId → { status, signature, wallet }`.
  In-memory with a TTL for v0; Postgres when pay-cloud gets persistence
  (milestone 2). The CLI does not depend on it: it also keeps the balance
  poll it has today.
- `GET /api/fund/{paymentId}`: `pending | disbursed { signature } | failed`.
- Config: `COINFLOW_API_KEY`, `COINFLOW_ENV` (`sandbox` | `prod`),
  `COINFLOW_MERCHANT_ID`, `COINFLOW_WEBHOOK_KEY`. No defaults for the key or
  the merchant; the server refuses to start the module without them.

### web-ui: `/fund` page

- Terminal theme like the rest of the CLI-linked pages: wordmark, the
  `$ pay topup` line, amount presets as terminal buttons, the fee table from
  `totals`, then `CoinflowPurchase` from `@coinflowlabs/react` with
  `sessionKey`, `checkoutJwtToken`, `env`, `merchantId`, `blockchain:
  "solana"`. Card entry stays in Coinflow's iframe; our page never sees the
  PAN. `onSuccess(paymentId)` switches to a progress log ("Charged $21.70",
  "Sending 20 USDC to CcZF…", "Confirmed: <explorer link>") fed by
  `GET /api/fund/{paymentId}`, then follows `callback` when present.
- In the onboarding flow, `ProviderCallback` goes to `/fund` after "Wallet
  created" instead of returning to the terminal, with a "Skip for now" line.
  Ledger and platform-keystore users reach the same page from the TUI.
- CSP: allow Coinflow's iframe and script origins for the SDK; nothing else
  changes.

### CLI: `pay setup`, `pay topup`, MCP `topup`

- "Buy stablecoins" opens `{cloud}/fund?address&callback&state` through the
  loopback module `cloud_onboard` already has (bind, PKCE not needed here,
  `state` only). It waits on either the callback (`payment_id`, `signature`)
  or the existing balance poll, whichever first, and prints the explorer
  link when it has a signature. `PAY_CLOUD_LOCAL` / `PAY_CLOUD_URL` apply as
  they do for onboarding. The mobile-wallet Solana Pay QR remains the second
  option.
- Because only the address is needed, the same code path serves local,
  Ledger and remote accounts. No signing happens during funding.
- MCP `topup` with `method: onramp` returns the `/fund` URL (and keeps the
  QR path). The provider-URL list becomes secondary.
- pay-api: `/v1/onramp/*` and `MoonpayConfig` are removed once the new path
  ships; the balance endpoint the TUI polls stays.

### A cheaper first step

The hosted checkout link with `callbackUrl` set to the CLI loopback works
today in sandbox with no web-ui work: Coinflow's own page, redirect back on
success. It is the right way to validate the money path end to end the day
Coinflow enables destination settlement. It is not the end state: Coinflow
branding, no fee preview in our style, and no shared page with the web
onboarding flow.

## Questions for Coinflow before building

1. Enable per-transaction USDC settlement (`destinationAuthKey` /
   `destination`) on the sandbox merchant `solana-foundation` and on the
   production merchant. Confirm the use case is allowed under the merchant
   agreement: customers buying USDC delivered to their own wallets, at face
   value, fees charged to the customer.
2. Disbursement timing after a card authorization: seconds, or after
   settlement and a hold? This decides the UX copy ("funds arrive in about
   a minute" versus "come back later") and whether the CLI should wait.
3. Does the destination need an existing USDC associated token account, or
   does Coinflow's fee payer create it? Fresh pay accounts have none.
4. Sandbox settlement: which network and mint, so the CLI can run an
   end-to-end test against devnet in CI.
5. Limits: per purchase, per card, per customer, per day; and what
   chargeback protection needs from us (`chargebackProtectionData`, the
   device script on our page).
6. Apple Pay and Google Pay domain verification for `cloud.pay.sh`.

## If Coinflow will not enable destination settlement

Settle to a pay-controlled wallet (pay-api's KMS fee payer already funds
accounts for `/v1/redeem`) and forward on the `Disbursed Funds` webhook keyed
by `webhookInfo.address`. It works with today's merchant configuration, but
pay becomes a pass-through custodian holding customer funds between two
transfers. That is a legal question before an engineering one, so it is the
fallback, not the plan.

## Milestones

- **F1** Ask Coinflow (questions above). In parallel: `funding` module in
  pay-cloud and the `/fund` page against sandbox settling to the merchant
  wallet, to get the UX right; TUI "Buy stablecoins" points at `/fund`
  behind `PAY_ONRAMP=coinflow`; MoonPay path untouched.
- **F2** Destination settlement enabled: lock `destination` in the JWT,
  webhook receiver, sandbox end to end from a fresh account, then one $2
  production purchase from a Ledger account.
- **F3** Remove MoonPay from pay-api and the TUI; MCP `topup` returns the
  `/fund` URL; docs and the setup screenshots.
