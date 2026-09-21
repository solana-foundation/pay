# pay-cloud

The hosted half of pay. One axum server, a separately deployed frontend, and four jobs:

- **Onboarding** for `pay setup --backend cloud`: the `gh auth login`
  pattern. The CLI follows a redirect to the pages app, which sends the browser through a
  wallet provider (Openfort), and the CLI redeems a one-time code with PKCE.
- **Funding**: the pages app's `/onramp` page buys USDC with a card through Coinflow for
  any address, used by the TUI and the web flow alike.
- **The MCP connector**: `/mcp` serves the pay tools over streamable HTTP
  to hosts like Grok, Claude and Cursor, behind an OAuth 2.1 authorization
  server this crate implements (`/.well-known/*`, `/oauth/*`).
- **Tenants**: a connector subject bound to a wallet and a spending policy.
  With Privy configured, the consent page signs the user in with Privy and
  the user's Privy wallet (pay's key as an additional signer) is the
  tenant's wallet; pay-cloud then holds only the app's operator
  credentials, nothing per user.

Everything below runs locally. State is in memory and lost on restart.

## Try it

```sh
rust/crates/cloud/dev/run.sh
```

Builds the payment-debugger bundle and the binaries, starts a mock Openfort so wallet
creation works with no Openfort account, loads the repo-root `.env`
(Coinflow sandbox), and starts pay-cloud on `http://127.0.0.1:8402` with
the connector on. It prints the commands to try: adding the connector to
Claude Code and authenticating through the consent page, `pay setup
--backend cloud`, and `pay topup` with a card. `--real-openfort` uses the
real dashboard; `--public-url` sets the issuer and allowed hosts for a
tunnel, which Grok needs.

Environment: `PAY_CLOUD_MCP=1` mounts `/mcp` and the OAuth server;
`PAY_CLOUD_MCP_TOKENS` adds comma-separated static bearer tokens;
`PAY_CLOUD_MCP_ALLOWED_HOSTS` overrides the `Host` allowlist derived from
the public URL. `COINFLOW_*` configure funding (see `docs/onramp-coinflow.md`).
`OPENFORT_BASE_URL` and `OPENFORT_AUTH_PAGE_URL` point the driver at a
mock or staging.

`PAY_CLOUD_PAGES_URL` points browser redirects at the pay.sh web app
(`/connect` there, which proxies `/api/oauth/*` and `/api/fund/*` back
here); it defaults to `https://pay.sh`.

Privy login on the consent page needs, from dashboard.privy.io:
`PRIVY_APP_ID` and `PRIVY_APP_SECRET` (App settings),
`PRIVY_AUTHORIZATION_PRIVATE_KEY` (a `wallet-auth:…` P-256 private key) and
`PRIVY_SIGNER_ID` (the key quorum registered for its public key; the
dashboard's Authorization keys page creates both, or `POST /v1/key_quorums`
with a locally generated key). Token signatures are checked against the
app's JWKS, fetched once at startup from auth.privy.io (`PRIVY_JWKS_URL` to
override, or `PRIVY_VERIFICATION_KEY` for a single PEM). Optional:
`PRIVY_POLICY_ID` to attach a wallet policy to wallets pay-cloud creates,
`PRIVY_API_BASE_URL` for a mock. The dashboard must also list the server's
origin under allowed origins. Put them in the repo-root `.env`; `dev/run.sh`
loads it.

## Frontend

The browser UI lives in the separate `solana-foundation/pay-web-ui`
repository and is deployed independently. Point local pay-cloud at it with
`PAY_CLOUD_PAGES_URL`.

This crate does not build, embed, or serve frontend assets.

## Run

```sh
cargo run -p pay-cloud -- --port 8402
```

Flags: `--bind` (default `127.0.0.1`), `--port` (default `8402`). Logging is
`RUST_LOG`-driven and goes to stderr.

Browser entrypoints redirect to `PAY_CLOUD_PAGES_URL`; unknown paths return
JSON 404 responses. `GET /health` returns `{"status":"ok"}`.

Try it end to end without the browser:

```sh
cargo run -p pay -- cloud-onboard --url http://127.0.0.1:8402
```

## JSON endpoints

### `POST /api/onboard/start`

Called by the page once the user submits an email.

```json
{
  "email": "a@b.co",
  "callback": "http://127.0.0.1:53211/callback",
  "state": "<16..128 base64url chars>",
  "code_challenge": "<43..128 base64url chars>",
  "account": "default",
  "host": "my-laptop",
  "cli": "0.29.0"
}
```

`callback` must be `http://127.0.0.1:<port>/callback` or
`http://localhost:<port>/callback` with no query string or fragment.
`account`, `host`, and `cli` are optional and informational.

Response `200`:

```json
{ "redirect": "http://127.0.0.1:53211/callback?code=<code>&state=<state>" }
```

`code` is 32 random bytes (base64url, no padding), valid for 5 minutes,
single use. Validation failures return `400`
`{ "error": "<code>", "message": "..." }` with `error` one of
`invalid_request`, `invalid_email`, `invalid_callback`, `invalid_state`,
`invalid_code_challenge`.

### `POST /v1/onboard/exchange`

Called by the CLI after the loopback listener receives the code.

```json
{ "code": "<code>", "code_verifier": "<verifier>" }
```

The server checks `base64url(sha256(code_verifier)) == code_challenge` and
consumes the session. Unknown, expired, reused, or mismatched codes return
`400 { "error": "invalid_grant", "message": "..." }`.

Response `200`:

```json
{
  "provider": "pay-cloud",
  "status": "pending",
  "email": "a@b.co",
  "network": "mainnet",
  "message": "Wallet provisioning is not available yet."
}
```
