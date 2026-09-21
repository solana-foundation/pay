# Pay Cloud: hosted connector, remote-wallet onboarding, and Ledger

Status: revision 2, milestone 1 in progress on branch `feat/pay-cloud`.
Owner: pay core team.

## Milestone 2 status (2026-09-16): the Grok connector

Done on `feat/pay-cloud`:

- **A3** `/mcp` in pay-cloud: rmcp streamable HTTP, a `PayMcp` per session,
  `Host` validation from the public URL, bearer middleware. Enabled by
  `PAY_CLOUD_MCP=1`; `PAY_CLOUD_MCP_TOKENS` adds static tokens for hosts
  that only take a header. Every refusal is a 401 with the RFC 9728
  `WWW-Authenticate resource_metadata` pointer.
- **A4** OAuth 2.1 authorization server in pay-cloud: both well-known
  documents, DCR accepting Grok's exact registration (public client,
  `https://grok.com/connectors/oauth/callback`), `/oauth/authorize` with
  mandatory S256 PKCE and RFC 8707 `resource`, a terminal-themed consent page
  at `/authorize`, `/oauth/token` for code and refresh (rotated) grants,
  `/oauth/revoke`. Opaque tokens stored hashed, 1 h access, 30 d refresh,
  in memory and bounded. Tested end to end into an MCP session; a live
  curl run against a local server did the same.
- **A2** `PayContext` in pay-mcp: every tool call resolves a `CallScope`
  (accounts store, overrides, approval policy, whether local files may be
  read). `LocalContext` reproduces `pay mcp`; pay-cloud's `CloudContext`
  resolves the bearer's tenant into a read-only one-account store whose
  credentials live in memory (`CredentialSource` on `AccountsStore`, the
  A1 injection seam) and a `PolicyApproval`: per-call ceiling and daily cap
  from the intent's exact amount (`AuthIntent::amount_minor_units`), then
  the client's elicitation when it has one. A subject with no wallet is
  told to finish setup.
- **A5, first half.** Consent binds a wallet. A browser with no wallet is
  offered the compiled-in providers on the consent page; the Openfort hop
  runs through the same driver as the CLI onboarding, with the OAuth
  request id as its `state`. Completing it provisions the wallet, binds a
  new subject to it in the `TenantRegistry` (credentials in memory, the
  default policy of $1 a call and $10 a day), approves the pending request
  for that subject, and sets a `pay_subject` cookie so the same browser
  reuses its wallet when it connects another client. Tested end to end:
  Grok registration, consent, wallet creation, code, token, an MCP `topup`
  naming the new wallet, then a second connection from the same browser
  approving straight away against the same wallet.
- **Grok, live (2026-09-17).** Through a Cloudflare quick tunnel, Grok's
  custom connector connected and listed the pay tools. What it took, and
  what it revealed about Grok's client:
  - Grok probes `/mcp`, reads both well-known documents, registers as a
    public PKCE client with redirect
    `https://grok.com/connectors-oauth-exchange-code/`, receives the 201,
    and then never opens the authorization endpoint. It shows "connector is
    unavailable". Other spec-compliant servers report the same; the missing
    step is on Grok's side or needs something undocumented in the 201.
  - Grok sends no `Authorization` header from its connector dialog, so a
    static token is not a way in either.
  - The demo therefore runs `run.sh --anonymous`: requests with no header
    act as one static token's tenant, bound to the mock wallet. Dev only,
    logged as a warning, never for a deployment.
  - Along the way the server gained what other hosts do need: path-based
    metadata locations, CORS, `client_secret_*` registration, the full
    RFC 6750 challenge with `scope`, and request logging at INFO.
  - Open for production: how a Grok user gets an identity. Options are
    xAI fixing the OAuth step (ask them, with the request log), or an
    in-band login where a tool hands out a link and the returned session is
    bound to a tenant, which needs Grok to keep a session across calls.
    The header names Grok sends are being logged to find any handle.
- **Identity and hosts (2026-09-17).** A tenant's subject is now derived
  from the provider account (`sha256(provider:project_id)`), not minted at
  random: a user who signs in to the same Openfort project from any browser
  gets the same wallet back, no second wallet is provisioned, a rotated API
  key is carried into the stored credentials (`WalletDriver::
  refresh_credentials`), and the `pay_subject` cookie is only a shortcut
  to that subject. A forged cookie names no tenant and is refused. The
  OAuth store takes a clock, and expiry of requests, codes, access and
  refresh tokens is tested on schedule.

  `hosts.rs` is the registry of MCP hosts as OAuth clients: Grok, Claude,
  ChatGPT/Codex, Cursor, Claude Code, Codex CLI, each with the redirect
  URIs it registers (exact HTTPS callbacks, per-connector prefixes,
  loopback paths, or a custom scheme such as `cursor://`) and observed
  quirks (dynamic registration, static header, whether OAuth completes).
  The redirect policy follows from it: HTTPS anywhere, HTTP on loopback,
  custom schemes only for a known host. Registrations record the host, and
  the consent page names it. Adding a host is one entry; the OAuth server
  itself has no per-host branches.
- **Privy as identity and wallet (2026-09-17).** Decision 11 below. The
  consent page signs the user in with Privy (`@privy-io/react-auth`, app id
  served by `GET /api/oauth/authorize/{request}` as `privy.app_id`) and
  posts the access token as `Authorization: Bearer` to `/approve`.
  pay-cloud verifies it offline (ES256, `PRIVY_VERIFICATION_KEY`, audience
  the app id, issuer `privy.io`), takes the DID as the tenant subject
  (`subject_for("privy", did)`), finds the user's embedded Solana wallet or
  creates one owned by the user with pay's key quorum as an additional
  signer and `PRIVY_POLICY_ID`, and binds the tenant. A wallet that predates
  pay and does not list pay's signer answers `409 signer_required` with the
  address; the page adds the signer with Privy's `useSigners().addSigners`
  (only the owner can) and retries. Signing goes through pay-core's new
  `privy` `RemoteProvider` (solana-keychain `PrivySigner`, fields `app_id`,
  `app_secret`, `authorization_key`), the same three operator credentials
  for every tenant. Tested end to end against a mock Privy: register, sign
  in, wallet created with the signer, code, tokens, `topup` naming the
  wallet; returning user by cookie and by token; forged token 401; foreign
  wallet 409. Live against a real Privy app: pending the dashboard set-up.
- **Pages move to pay-web-ui (2026-09-18).** The consent page is
  `/connect` in the pay.sh Next.js app (branch `feat/connect` there), with
  a headless Privy sign-in (`useLoginWithEmail`: our own email and code
  inputs, no Privy modal) and `/api/connect/*` route handlers proxying to
  pay-cloud, forwarding `Authorization` and `Cookie` in and `Set-Cookie`
  out so `pay_subject` lives on pay.sh. A new wallet continues to that
  app's `/onramp` (Coinflow React SDK, card and Apple/Google Pay) with
  `request` and `client`, which approves the pending request when funded
  or skipped. pay-cloud sends users there through `PAY_CLOUD_PAGES_URL`
  (default `https://pay.sh`) and does not embed or serve a frontend.
- **Guests (2026-09-18).** The consent page offers "Continue as guest":
  Approve with `{"guest": true}` mints a wallet-less `guest_…` subject and
  the host connects normally. Catalog tools need no wallet; the first tool
  call that would pay (`curl`, `get_balance`, `topup`) fails with a message
  carrying a one-time link, `{pages}/connect?link=<ticket>` (30-minute
  ticket, hash stored in the `TenantRegistry`). That page signs the user in
  with Privy and `POST /api/oauth/link/{ticket}` attaches the wallet to the
  guest subject (and binds it under the Privy subject, cookie set), so the
  host's existing tokens start paying. An empty wallet continues to the
  onramp, then the user goes back and asks again. Sign-up and funding thus
  happen the first time money is needed, not at connection time.
- Not yet: a `/connect` page for limits and revocation, Redis for OAuth
  state and MCP sessions across replicas, funding from inside the consent
  flow (today `topup` hands the user the `/fund` URL), deployment, the
  xAI report (`docs/grok-connector-report.md`) sent.

## Milestone 1 status (2026-09-15)

Done on `feat/pay-cloud` (PR #464, rebased on main after PR #423 merged on 2026-09-16):

- Keychain cleanup: `pay_core::backend` registry and `SigningBackend`
  capability trait; `RemoteProvider` is a supertrait so Circle is a file
  plus a registry line. See `docs/keychain.md`. 1Password is deprecated:
  existing accounts load, new ones are refused.
- `pay-cloud` v0 (`crates/cloud`): serves `POST /api/onboard/start` and
  `POST /v1/onboard/exchange`
  with PKCE S256, single-use five-minute codes. In-memory only.
- Browser pages live in the separate `solana-foundation/pay-web-ui`
  repository and are deployed independently from this service.
- `pay setup --backend cloud` and the "Remote wallet" picker entry run the
  loopback flow end to end; the exchange returns `status: pending` because
  provisioning is not built. `PAY_CLOUD_LOCAL=1` targets `http://127.0.0.1:8402`, `PAY_CLOUD_URL` any other server,
  `PAY_NO_BROWSER=1` skips opening the browser.

- Openfort onboarding driver (`pay_cloud::drivers::openfort`, `openfort`
  cargo feature, on by default): the page's "Continue with Openfort" sends
  the browser to Openfort's own consent page; the fragment comes back to
  `/onboard/openfort/callback`; the driver registers a fresh wallet secret,
  records its public key on the project, creates a Solana backend wallet,
  and the exchange hands the CLI `{secret_key, wallet_secret, wallet_id,
  pubkey}`. The CLI verifies the address with Openfort and registers a
  normal `--backend openfort` account. pay-cloud keeps nothing past the
  five-minute session.

### Decision change (2026-09-15): pay owns no custody account

The earlier revision had pay-cloud own one Openfort project and a backend
wallet per tenant, which made pay-cloud a custodian with a policy engine.
Ludo's direction is the opposite: sign users up with the provider
programmatically and hand them their own project. Openfort's dashboard
already exposes the pieces its CLI uses (`/oauth/consent` returning the
project's keys, `register-secret`, `PUT /v1/project/apikey`, `POST
/v2/accounts/backend`), so the driver replaces six manual steps with one
browser hop. Consequences:

- Track A5 (tenant store, per-tenant wallets, policy caps) and A6 (signer
  API) are dropped for the CLI path. The CLI signs with its own Openfort
  credentials through PR #423's provider, gated by Touch ID as before.
- The hosted MCP connector (Track A3, A4) still needs a place to sign for
  a browser-only user. That becomes: the same Openfort project credentials
  stored per tenant, which does make the connector a credential custodian
  again. Decide before building A3 whether the connector keeps per-tenant
  provider credentials or whether the CLI remains the only signer.
- `OPENFORT_BASE_URL` and `OPENFORT_AUTH_PAGE_URL` (Openfort's own env
  names) point the driver and pay-core's provider at staging or a mock.

Open, to confirm with Openfort: whether `/oauth/consent` accepts a
non-loopback `redirect_uri` (Openfort's CLI registers `127.0.0.1`), the
exact `POST /v2/accounts/backend` request shape for SVM (the SDK sends
`{ chainType: "SVM" }`), and whether `register-secret` accepts the CLI's
JWT shape from a server. All three are verified only against a mock.

Next: run the flow against a real Openfort project, then decide the
connector custody question above.

### Ledger status (2026-09-15)

- pay is on pay-kit `0626cd7c` (pay-kit main, PR #323 merged) with solana-keychain
  `6461a18`, via a cherry-pick of pay PR #462 onto `feat/pay-cloud`. Every
  signer implements keychain 2.x `TransactionSigner`.
- `remote::ledger` provider behind the `ledger` feature; V0 cap and
  raw-message guards in place. See `docs/keychain.md`.
- Verified on a physical device (2026-09-16): `pay account new --backend
  ledger` reads the address from the device; a v0 MPP charge to
  debugger.pay.sh and a 0.045 USDC Gemini image generation on mainnet were
  both confirmed on the Ledger screen. The Gemini gateway advertises an
  operator-signed session first, which a Ledger cannot sign; the client now
  carries the flat charge as a fallback and `RunOutcome::for_account` picks
  it (see `docs/keychain.md`).
- Release builds (`release-cli.yml`) compile with `--features ledger` on
  every target, with `libudev-dev` installed natively and through a cross
  `pre-build` hook; CI lints and tests the feature on Linux and checks it
  on Windows.
- Two rough edges seen on the device run live in `solana-remote-wallet`
  (Agave). Its Trezor-bridge probe logs a refused connection on every
  connect: pay's default log filter now turns that crate off, since its
  failures reach the user as pay errors anyway. It also prints "Waiting for
  your approval on Ledger …" to stdout, which lands in front of a piped
  response body: fixed upstream by switching those prompts to stderr
  (Agave PR pending); pay picks it up with the next solana-remote-wallet
  release.
- Not yet done: the envelope-aware `signature_type` at the spec level
  (Track C6).

## Goals

1. **Hosted connector.** A user adds `https://mcp.pay.sh` to Grok Bot, Claude,
   ChatGPT, or Cursor, signs in once, funds an address, and their agent gets
   the seven Pay tools with no CLI and no key material on the host.
2. **Remote wallet in `pay setup`.** A new backend, "Remote wallet", opens
   `cloud.pay.sh` in the browser. The page handles sign-in, custody backend
   choice (Openfort now, Circle later), wallet provisioning, onramp, and
   spending caps, then hands the CLI what it needs to sign locally. The TUI
   onramp is skipped for this backend.
3. **Ledger.** Finish the hardware-wallet work started in pay-kit so a Ledger
   is a first-class `pay` account for every flow a device can physically sign.

Non-goals for this revision: physical goods, non-Solana networks, replacing
the local Keychain backends, and policies richer than a per-call ceiling plus
a daily cap.

## One service, two front doors

```text
              ┌──────────────── cloud.pay.sh (pay-cloud, Cloud Run) ────────────────┐
MCP hosts ───►│ /mcp            streamable HTTP MCP, OAuth 2.1 bearer                │
              │ /oauth/*        DCR, PKCE, token, revoke; /.well-known/*             │
pay setup ───►│ /onboard/*      sign-in, backend picker, provision, fund, caps       │
(browser)     │ /v1/onboard/*   one-time code exchange, headless poll                │
pay CLI ─────►│ /v1/wallets/*   policy-checked sign-transaction / sign-message       │
              │ /connect/*      receipts, policy, keys, withdraw                     │
              │ Postgres        tenants, wallets, policies, tokens, receipts         │
              │ Redis           MCP sessions, channel caches, rate limits            │
              └──────────────┬───────────────────────────────────────────────────────┘
                             │ CustodyBackend trait
                   ┌─────────┴─────────┐
                   │ Openfort (now)    │  one project, one backend wallet per tenant
                   │ Circle  (later)   │  developer-controlled wallets
                   └─────────┬─────────┘
                             ▼ Solana
```

The MCP connector and the CLI's remote wallet are the same tenant, the same
wallet, and the same policy. A paid call from Grok Bot and a `pay curl` from
the laptop both go through `/v1/wallets/{id}/sign-*` semantics, enforced in one
place.

## Trust model

| Party | Holds | Can do |
| --- | --- | --- |
| Custody backend | The tenant's private key in a TEE | Sign when presented with valid project credentials |
| pay-cloud | Project-level backend credentials from Secret Manager. Per tenant: wallet id, pubkey, policy, receipts, hashed tokens | Request a signature for any tenant wallet, subject to its own policy check |
| CLI (`pay setup --backend remote`) | A tenant-scoped API token in the platform keystore, Touch ID gated like any account | Ask pay-cloud to sign; cannot exceed the tenant's caps |
| MCP host | An OAuth access token for one tenant | Call tools; paid calls are policy-checked |
| User | A sign-in identity and a funding address, no secret | Set caps, read receipts, revoke, withdraw |

**Why there is no KMS.** Openfort's credentials are project-level. One
`sk_live_` secret key and one wallet-auth P-256 key authenticate the service
for every `acc_…` wallet in the project. Nothing per-tenant is secret, so the
database holds only wallet ids, pubkeys, policy rows, and hashed tokens. The
two service secrets live in Secret Manager or Doppler exactly like pay-api's
fee payer today. Circle is the same shape: an API key and an entity secret,
project-wide. KMS would only matter if we stored per-tenant secrets, and we do
not.

**Why the CLI never receives Openfort credentials.** For the same reason: the
credentials are project-wide. Handing them to one user's CLI would let that
user sign for every other tenant's wallet. So "the data needed for using
Openfort locally" is not Openfort's data. It is a pay-cloud tenant token plus
the wallet id and pubkey, and the CLI signs through pay-cloud, which signs
through Openfort. This is PR #423's `RemoteProvider` with a second provider,
`paycloud`, beside `openfort`. Users who own an Openfort project can still use
`--backend openfort` directly; that path is unchanged.

**Where policy is enforced.** pay-cloud is the only holder of the project
credentials, so it is the enforcement point for every signature: per-call
ceiling, daily cap, mainnet only, and a URL allowlist for MCP calls. Every
signature request carries an intent (URL, amount, currency, protocol) that is
checked and written to receipts. A backend's own policy engine is defense in
depth, never the primary control. Locally, Touch ID still gates the token
before any request leaves the machine, so a laptop user has two layers.

**Approval.** MCP hosts that render elicitation get a per-call prompt through
the existing `ElicitationAuth`. Hosts that do not get policy mode. The CLI
gets Touch ID plus policy. Setting the daily cap to zero on `/connect/policy`
freezes a tenant without moving funds.

## Decisions

1. **New crate `crates/cloud`, package `pay-cloud`.** pay-api stays the
   stateless balance and settlement service; tenant state and long-lived MCP
   sessions have a different failure profile.
2. **Postgres for tenant state, Redis for sessions and counters.**
3. **`CustodyBackend` trait inside pay-cloud** with `create_wallet`,
   `address`, `sign_transaction`, `sign_message`. Openfort first, Circle
   second. This is server-side and distinct from pay-core's `RemoteProvider`,
   which is the client-side view.
4. **`paycloud` as a pay-core `RemoteProvider`.** Credential field: one
   `api_token`. `connect` builds a `PayCloudSigner` implementing
   `SolanaSigner` and `TransactionSigner` over HTTPS.
5. **Loopback callback for browser to CLI**, the pattern `gh auth login` and
   `gcloud auth login` use, with a device-code poll for headless hosts. I
   could not find an equivalent flow in surfpool to reuse; the pattern below
   is the standard one.
6. **Own the OAuth 2.1 authorization server.** Grok's connector UI requires
   PKCE plus Dynamic Client Registration and cannot attach a static header.
   rmcp 1.8 ships only the client side. Five endpoints, tested against the
   exact registration Grok sends.
7. **Sign-in with Solana as the identity.** The user needs a wallet to fund
   and withdraw, and the same signature later powers the non-custodial
   allowance flow.
8. **`PayContext` in pay-mcp.** Tools stop reading the accounts file and
   `PAY_*` env vars directly. The stdio server injects a local context; the
   cloud injects a tenant context.
9. **Ledger is a `RemoteProvider` too**, id `ledger`, with no credential
   fields. The registry becomes the single place a backend is added,
   whether the key is in a TEE, a cloud, or a USB device.
10. **Connector custody: pay-cloud stores per-tenant Openfort credentials
    (2026-09-16).** A browser-only host has no CLI and no Touch ID, so
    something server-side must sign. The credentials come from the same
    consent driver the CLI onboarding uses, encrypted at rest, and policy is
    the only control on their use. Chosen over one pay-owned project with a
    wallet per tenant (rejected on 2026-09-15) because it reuses M1 whole and
    keeps each user on their own Openfort project. Build order for the Grok
    connector: A3 transport, A4 OAuth, A2 tenant context, A5 tenant store.
    A6 (the CLI signing through pay-cloud) is not needed for the connector.
11. **Connector identity and wallet at Privy; no database for now
    (2026-09-17).** Supersedes 10 for the connector. Per-tenant Openfort
    credentials made pay-cloud a custodian of every user's project secret
    and forced an encrypted store. With Privy the user owns the wallet,
    pay's authorization key is one additional signer constrained by the
    user's grant and Privy's policy, identity is Privy's signed token, and
    pay-cloud holds only the app's operator credentials. The remaining
    state (OAuth clients, codes, tokens, the daily spend counter) is short
    lived and stays in memory behind the existing bounded stores; Redis
    when there is more than one replica. The CLI path is unchanged: a
    user's own Openfort project and local keychain. Openfort's embedded
    wallet mode would fit the same shape if one vendor is preferred.

## Track A: pay-cloud service

### A1 Land PR #423 and add credential injection

- Merge #423. It is the signer seam for every backend below.
- Add `load_remote_signer_with_credentials(account, name, network,
  credentials)` beside `load_remote_signer`, so a server can pass service
  credentials instead of reading a platform keystore blob.
- Add `connect_async` to `RemoteProvider`, keeping the sync wrappers for the
  CLI. Both `discover` and `connect` currently spin a throwaway runtime.
- Relax `registered_providers_are_well_formed`: a provider may declare zero
  credential fields (Ledger). Add `fn requires_credentials(&self) -> bool`.
- Add `create_wallet` to the trait with a default `Unsupported` error.

### A2 `PayContext` in pay-mcp

```rust
pub trait PayContext: Send + Sync {
    fn accounts(&self) -> Arc<dyn AccountsStore>;
    fn network_override(&self) -> Option<String>;
    fn account_override(&self) -> Option<String>;
    fn auth_gate(&self, peer: Option<&Peer<RoleServer>>, intent: &AuthIntent) -> AuthOverride;
    fn session_cache(&self) -> Arc<SessionCache>;
    fn allows_body_file(&self) -> bool;
    fn rpc_url(&self, network: &str) -> String;
}
```

- `LocalContext` reproduces today's behavior; `PayMcp::new()` keeps using it.
- Thread the context through `curl`, `get_balance`, `topup`. The catalog tools
  are tenant-independent.
- `PolicyGate` as an `AuthGate` beside `ElicitationAuth`: checks the intent's
  amount against `per_call_ceiling` and remaining `daily_cap`, records spend
  on success. The cloud context returns `ElicitationAuth` when the peer
  advertises elicitation, else `PolicyGate`.
- Assert a multi-thread runtime: `ElicitationAuth` uses `block_in_place`.

### A3 Crate, transport, sessions

- `crates/cloud`: axum, tower-http, rmcp with `server-side-http`, sqlx
  (postgres, rustls), redis, figment YAML plus env, OpenTelemetry. Bootstrap
  mirrors `pay-api/src/main.rs`.
- `/mcp`: `StreamableHttpService` with `stateful_mode = true`,
  `allowed_hosts = ["mcp.pay.sh"]`, keep-alive 15 s. A middleware validates
  the bearer, loads the tenant, and inserts a `TenantHandle` into request
  extensions; `CloudContext` reads it from the `http::request::Parts` rmcp
  injects into each tool call. Reject a call whose tenant differs from the
  one that opened the session.
- Redis `SessionStore` for rmcp sessions; per-tenant MPP authorizations and
  x402 batch channels also move to Redis so a channel opened on one instance
  is reused on the next.

### A4 OAuth 2.1 authorization server

| Path | Purpose |
| --- | --- |
| `GET /.well-known/oauth-authorization-server` | PKCE `S256`, `code` and `refresh_token`, `none` client auth, registration endpoint |
| `GET /.well-known/oauth-protected-resource` | RFC 9728, points `/mcp` at this AS |
| `POST /oauth/register` | DCR; accept `token_endpoint_auth_method: none`, `https://` or loopback redirects |
| `GET /oauth/authorize` | Validate client and redirect, require `code_challenge`, hand off to sign-in and consent |
| `POST /oauth/token` | Code exchange with PKCE; refresh with rotation |
| `POST /oauth/revoke` | Revoke access and refresh tokens |

Opaque random tokens stored hashed, 1 h access and 30 d refresh. `/mcp`
without a token returns `401` with `WWW-Authenticate: Bearer
resource_metadata=…`. Static API tokens minted on `/connect/keys` serve hosts
that only take headers and the CLI's `paycloud` provider. A conformance test
replays Grok's exact DCR body (`client_name: "Grok"`, redirect
`https://grok.com/connectors/oauth/callback`, `token_endpoint_auth_method:
"none"`).

### A5 Tenant store and custody

Migrations in `crates/cloud/migrations/`:

```text
tenants(id, siws_pubkey, created_at, disabled_at)
wallets(tenant_id, backend, backend_wallet_id, pubkey, network, created_at)
policies(tenant_id, per_call_ceiling_usd, daily_cap_usd, mode, updated_at)
spend_ledger(tenant_id, day, spent_usd)
receipts(id, tenant_id, source, url, protocol, amount_usd, currency, signature, ts)
api_tokens(token_hash, tenant_id, label, created_at, revoked_at)
oauth_clients(client_id, redirect_uris, created_at)
oauth_codes(code_hash, client_id, tenant_id, pkce_challenge, expires_at)
oauth_tokens(token_hash, kind, client_id, tenant_id, expires_at, revoked_at)
onboard_codes(code_hash, tenant_id, pkce_challenge, callback, device_code, expires_at, consumed_at)
```

- `CustodyBackend` trait with `OpenfortBackend` first. It wraps the same
  `OpenfortSigner` PR #423 uses, plus account creation.
- `DbAccountsStore` implements `AccountsStore` for one tenant: one `mainnet`
  account with `keystore: remote, provider: openfort, account: acc_…`; `save`
  writes back only the `subscriptions` map.
- Default policy on provisioning: per call 0.10 USD, daily 5 USD, mode
  explicit-else-policy. Spend accounting and receipt insert happen in one
  transaction under a per-tenant advisory lock.

### A6 Signer API for the CLI

```text
POST /v1/wallets/{id}/sign-transaction
  { "message_b64": "<VersionedMessage bytes>",
    "intent": { "url": "...", "amount_usd": "0.02", "currency": "USDC", "protocol": "mpp" } }
→ { "signature_b58": "..." }

POST /v1/wallets/{id}/sign-message
  { "message_b64": "...", "intent": { ... } }
→ { "signature_b58": "..." }
```

- Bearer is a tenant API token. The wallet must belong to the tenant.
- Before signing a transaction, pay-cloud decodes the message and verifies the
  stablecoin transfer amount against the intent and the policy, using
  pay-kit's existing charge verification helpers. A message that moves more
  than the intent claims is refused.
- `sign-message` is allowed only for known MPP and x402 proof shapes (SIWMPP,
  session proof, subscription proof, batch voucher), each parsed and bounded.
  Arbitrary bytes are refused.
- Every success writes a receipt with `source = cli`.

### A7 Onboarding endpoints and pages

- `GET /onboard?callback&state&code_challenge&account&host&cli` starts the
  flow described in Track B. Pages: sign-in (SIWS or fresh), backend picker,
  provisioning, fund (Solana Pay QR plus the existing pay-api MoonPay redirect
  with `walletAddress` prefilled and a `redirectURL` back into the flow),
  caps, done.
- `POST /v1/onboard/exchange { code, code_verifier }` returns the
  `OnboardResult` below once, then consumes the code.
- `GET /v1/onboard/poll?device_code=` for headless hosts; returns `pending`
  until the user finishes, then the same `OnboardResult` once.
- `/connect/*` pages: receipts, policy, keys, withdraw. Withdraw signs a full
  balance transfer to the SIWS wallet through the same backend.

### A8 Ops

Dockerfile from pay-api's, Cloud Run with min instances 1, Cloud SQL, the
existing Redis. Rate limits per tenant and per client in Redis. Spans per
tool call and per sign request. Deny loopback and private-network URLs in
`curl`, cap response bodies, cap concurrent paid calls per tenant at 2.
Runbook: rotate backend secrets, freeze a tenant, replay a receipt.

## Track B: `pay setup --backend remote`

### B1 Loopback callback protocol

```text
CLI
  state         = random 32 bytes, base64url
  code_verifier = random 32 bytes, base64url
  code_challenge = base64url(sha256(code_verifier))
  bind 127.0.0.1:0, serve GET /callback
  open https://cloud.pay.sh/onboard
        ?callback=http://127.0.0.1:PORT/callback&state=…&code_challenge=…
        &account=<name>&host=<hostname>&cli=<version>
  print "Complete setup in your browser… (press q to cancel)"

Browser
  sign in → pick backend → provision → fund (optional, skippable) → caps
  → 302 http://127.0.0.1:PORT/callback?code=…&state=…
  → page says "Return to your terminal"

CLI
  verify state; POST /v1/onboard/exchange { code, code_verifier }
  → OnboardResult
  store api_token as a credential blob (Touch ID gated, PR #423 path)
  write accounts.yml entry; print success with balance from pay-api
```

```json
{
  "provider": "paycloud",
  "wallet_id": "w_01J…",
  "pubkey": "7xKX…",
  "network": "mainnet",
  "api_token": "pct_…",
  "policy": { "per_call_usd": "0.10", "daily_usd": "5.00" },
  "funded": { "usdc": "5.00", "tx": "…" }
}
```

- The callback listener reuses axum, already a CLI dependency for the payer
  proxy. Bind to `127.0.0.1` only, accept exactly one request, 10 minute
  timeout, `q` cancels.
- The code is single-use, five minutes, bound to the PKCE challenge. The
  callback URL and `state` are checked server-side against what started the
  flow so a code cannot be redirected.
- `--no-browser` prints the URL and a short device code and polls
  `/v1/onboard/poll`. Also the automatic path when no browser can be opened
  or stderr is not a TTY.

### B2 `paycloud` RemoteProvider in pay-core

- `core/src/remote/paycloud.rs`: `id = "paycloud"`, one credential field
  `api_token`, `discover` lists the tenant's wallets via
  `GET /v1/wallets`, `connect` returns `PayCloudSigner`.
- `PayCloudSigner` implements `SolanaSigner` and `TransactionSigner`: caches
  the pubkey, posts to `/v1/wallets/{id}/sign-transaction` and `sign-message`,
  verifies the returned signature locally before use. Base URL from
  `PAY_CLOUD_URL`, default `https://cloud.pay.sh`.
- Intent metadata: pay-core's builders know the URL, amount, and protocol
  when they sign. Thread an `Option<SignIntent>` into the signer through the
  existing `AuthIntent` that already carries amount and description.
- `accounts.yml` entry, using PR #423's schema:

  ```yaml
  accounts:
    mainnet:
      ludo:
        keystore: remote
        provider: paycloud
        account: w_01J…
        pubkey: 7xKX…
        auth_required: true
  ```

### B3 Setup and account changes

- `pick_backend` gains "Remote wallet (cloud.pay.sh)" and "Ledger hardware
  wallet" entries, drawn from `pay_core::remote::providers()` after the
  platform entry. `--backend remote` and `--backend ledger` flags.
- `setup.rs`: when the backend is `remote`, run the B1 flow, skip
  `run_topup_flow`, and print success from `OnboardResult.funded`. When
  funding was skipped, print the existing "top-up required" notice with
  `pay topup` pointing at the cloud fund page.
- `account new --backend remote` runs the same flow for an additional
  account. `account destroy` revokes the token at pay-cloud, then deletes the
  blob. `account export` keeps refusing for remote accounts.
- `pay topup` for a `paycloud` account opens `/connect/fund` instead of the
  TUI.

### B4 Circle later

`CircleBackend` implements `CustodyBackend` with developer-controlled wallets:
API key plus entity secret ciphertext, both project-wide, wallet creation and
sign endpoints per wallet id. The onboarding backend picker lists it once the
implementation lands. Nothing in the CLI changes; the `paycloud` provider is
backend-agnostic.

## Track C: Ledger

### C1 What exists

- solana-keychain at `6461a18` ships a `ledger` feature: `LedgerSigner`
  over `solana-remote-wallet` and `hidapi`, one serialized device actor,
  bounded timeouts, auto-open of the Solana app, and a derivation path
  config. `sign_transaction` sends the serialized message to the device and
  the signature verifies like a software backend's for legacy, v0, and v1
  (v1 falls back to blind signing until the app supports it). `sign_message`
  wraps the payload in a Ledger off-chain envelope, so the signature covers
  the envelope, not the raw bytes; `ledger_offchain_envelope` rebuilds it for
  verifiers.
- pay-kit commit `7efc59b3` forwards a `ledger` feature from the kit to the
  keychain. It sits on branch `feat/servers-accept-legacy-tx`, not main.
- pay-kit branch `pr-206-ledger-charge-sign-transaction` moved charge and
  subscription activation from `sign_message` to `sign_transaction`. Main has
  since routed both through `core::signing::sign_versioned_transaction_slot`
  (#300, #319), so the branch is superseded. Its equivalence test is worth
  porting; the branch can be closed.
- pay-kit builders already accept `max_tx_version` (commit `2847fdf6`), added
  for exactly this: Ledger-backed clients pass `Some(V0)`.
- pay PR #462 (`feat/tx-v1`) pins kit `733fe68b`, moves all ten sign sites to
  `core::signing`, and is the branch Ledger support must build on.

### C2 What a Ledger cannot sign today

Every client path that calls `sign_message` on the wallet key produces a raw
ed25519 signature the server verifies over the raw bytes. A Ledger signs the
envelope instead, so verification fails. Affected today:

| Flow | Call site | v1 behavior for Ledger |
| --- | --- | --- |
| SIWMPP authenticate | kit `mpp/client/authenticate.rs:100` | Unsupported, clear error |
| Subscription bearer proof | kit `mpp/client/subscription.rs:85` | Activation works; reuse proof unsupported, so subscriptions are unsupported until C6 |
| Operator-signed session proof | pay `core/src/client/session.rs:405` | Unsupported; client-signed sessions work because the voucher key is ephemeral |
| x402 batch voucher | kit `x402/client/batch_settlement/payment.rs:361` | Unsupported when the wallet is the authorizer |
| MPP charge, x402 exact and upto, session open, subscription activation | transaction signing | Supported |

### C3 pay-kit steps

1. Cherry-pick `7efc59b3` onto main as its own PR. Do not merge
   `ef06c166` (servers accept legacy) with it: the 2026-09-11 decision drops
   legacy everywhere, and the Ledger app signs v0, so legacy is not needed.
2. Close pr-206 as superseded; port its
   `sign_transaction_matches_manual_message_signature` test to main.
3. Publish keychain `2.0.0-beta.2` so the kit can drop its git pin.

### C4 pay steps

1. Land PR #462, then bump the kit pin to a rev that includes the `ledger`
   feature and enable `pay-kit/ledger` behind a `ledger` cargo feature in
   `pay-core` and `pay`. Default on for release binaries; document libudev and
   the udev rule on Linux.
2. `core/src/remote/ledger.rs`: `id = "ledger"`, no credential fields,
   `discover` enumerates attached devices and the first few derivation paths
   (`m/44'/501'/0'`, `m/44'/501'/0'/0'`, `m/44'/501'/1'`), `connect` calls
   `LedgerSigner::connect_with` with `auto_open_app = true`. `accounts.yml`:
   `keystore: remote, provider: ledger, account: "m/44'/501'/0'", pubkey`.
   `auth_required` is ignored; the device is the gate.
3. `SignerCapabilities { raw_message: bool, max_tx_version: TxVersion }` on
   `ResolvedSigner`. Memory, openfort, paycloud: raw true, V1. Ledger: raw
   false, V0. `choose_payment` filters out options that need a raw message
   when the signer cannot provide one, and every builder call passes
   `max_tx_version` from the capabilities. Today pay-core passes no cap.
4. Intent messages: "Confirm on your Ledger" replaces the biometric prompt
   text; map the keychain's busy and timeout errors to actionable notices.
5. Verify the runtime boundary: pay-core builders block inside
   `spawn_blocking` with throwaway current-thread runtimes, and
   `LedgerSigner` uses `spawn_blocking` plus an actor thread. Add a test that
   signs through the full `pay curl` path with the keychain's fake device.
6. `pay setup --backend ledger` and `pay account new --backend ledger`,
   including the TUI top-up against the device's address.

### C5 Distribution

`hidapi` links against IOKit on macOS, hid on Windows, and libudev on Linux.
Add the feature to the Homebrew formula and the npm binary build, extend the
CI matrix, and keep a `--no-default-features` path that excludes it for
headless builds.

### C6 Phase 2: envelope-aware proofs

Make the `sign_message` flows work with hardware wallets at the spec level.
SIWMPP's payload already carries `signature_type`; add
`ed25519-offchain-ledger`, and have servers rebuild the envelope with
`solana_keychain::ledger_offchain_envelope` before verifying. Payloads must be
printable ASCII to avoid the app's blind-signing setting: SIWMPP text already
is; session, subscription, and batch proofs need an ASCII encoding variant.
This is an mpp-specs and x402 spec change plus pay-kit server work, and it
lifts every "unsupported" row in C2.

## Sequencing

| Week | Track A | Track B | Track C |
| --- | --- | --- | --- |
| 1 | A1 merge #423 plus injection; A2 `PayContext` | | C3: cherry-pick `ledger` feature to kit main, close pr-206 |
| 2 | A3 transport; A5 store and Openfort backend; A6 signer API; static bearer | B2 `paycloud` provider against a local pay-cloud | C4.1 land #462, bump kit pin |
| 3 | A7 onboarding pages and exchange; A4 OAuth AS | B1 loopback flow; B3 setup changes; skip TUI onramp | C4.2 to C4.4 ledger provider, capabilities, prompts |
| 4 | A3 Redis sessions; `/connect/*`; A8 ops; deploy | B1 headless poll; `pay topup` cloud path | C4.5 runtime test; C4.6 setup; C5 build matrix |
| 5 | B4 Circle backend | | C6 spec drafts |

Exit tests per milestone:

- **M1, end of week 2.** rmcp client over HTTP does `search_catalog` then a
  paid `curl` against `debugger.pay.sh` with a static bearer; receipt row
  written; cap enforced. `pay --account cloud curl` signs through
  `/v1/wallets/{id}/sign-transaction` from a laptop.
- **M2, end of week 3.** `pay setup --backend remote` completes in the browser
  and returns to a funded, Touch ID gated account with no TUI. A Grok
  connector added by URL completes OAuth and makes a paid call.
- **M3, end of week 4.** Instance restart mid-session keeps the MCP session.
  `pay setup --backend ledger` on a device, then a paid MPP charge and an
  x402 exact payment on devnet, each confirmed on the Ledger screen.
- **M4, week 5.** Circle listed in the backend picker; envelope spec PRs open.

One engineer runs Track A; a second runs B then C, since B is small and C
waits on #462. About five weeks total, four with the Circle backend and the
spec drafts deferred.

## Open questions to verify before week 2

1. Openfort: programmatic creation of Solana backend wallets under one
   project, and whether its policy engine expresses SVM spend caps.
2. Circle: developer-controlled wallets on Solana expose sign-transaction and
   sign-message per wallet; confirm before listing it.
3. Grok Bot: whether the connector dialog accepts a static bearer or requires
   OAuth, and whether it renders elicitation. Public guides disagree.
4. rmcp: `SessionStore` restore across two Cloud Run instances behind one load
   balancer. Fallback is session affinity.
5. Ledger: confirm x402 batch-settlement's `payer_authorizer` is the wallet
   and not an ephemeral key; if ephemeral, that row in C2 becomes supported.

## Risks

- **Hosted custody is custody.** pay-cloud can request any signature for any
  tenant. Mitigations: caps in code, receipts, withdraw, and the
  non-custodial allowance phase as the exit.
- **OAuth surface.** Public-client profile only, PKCE required, exact redirect
  matching, conformance test in CI.
- **Blocking calls in a shared server.** Bound the blocking pool and
  per-tenant concurrency.
- **Ledger UX.** A walked-away prompt occupies the device until it auto-locks;
  surface the busy error clearly and never queue behind it.
- **Feature lock-step.** Track C depends on #462 and a kit pin bump; if #462
  slips, C slips with it, and A and B are unaffected.
