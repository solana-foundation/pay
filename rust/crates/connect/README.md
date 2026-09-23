# pay-connect

The hosted half of pay. One axum server, a separately deployed frontend, and four jobs:

- **CLI linking** for `pay setup --backend connect`: the `gh auth login`
  pattern. The CLI follows a redirect to the shared `/connect` page, the user
  signs in with Privy, and the CLI redeems a one-time code with PKCE for a
  tenant-scoped pay-connect token.
- **Funding**: the pages app's `/onramp` page buys USDC with a card through Coinflow for
  any address, used by the TUI and the web flow alike.
- **The MCP connector**: `/mcp` serves the pay tools over streamable HTTP
  to hosts like Grok, Claude and Cursor, behind an OAuth 2.1 authorization
  server this crate implements (`/.well-known/*`, `/oauth/*`).
- **Tenants**: a connector subject bound to a wallet and a spending policy.
  With Privy configured, the consent page signs the user in with Privy and
  the user's Privy wallet (pay's key as an additional signer) is the
  tenant's wallet. Privy operator credentials stay in pay-connect and are
  never returned to the browser or CLI.

Everything below runs locally. State is in memory and lost on restart.

## Try it

```sh
rust/crates/connect/dev/run.sh
```

Builds the binaries, loads the repo-root `.env`, and starts pay-connect on
`http://127.0.0.1:8402` with the connector on. It prints the commands to try:
adding the connector to Claude Code, authenticating through the consent page,
`pay setup --backend connect`, and `pay topup` with a card. `--public-url`
sets the issuer and allowed hosts for a tunnel, which Grok needs.

Environment: `PAY_CONNECT_MCP=1` mounts `/mcp` and the OAuth server;
`PAY_CONNECT_MCP_TOKENS` adds comma-separated static bearer tokens;
`PAY_CONNECT_MCP_ALLOWED_HOSTS` overrides the `Host` allowlist derived from
the public URL. Coinflow checkout configuration and APIs live in `pay-web-ui`.

`PAY_CONNECT_PAGES_URL` points browser redirects at the pay.sh web app
(`/connect` there, which proxies `/api/oauth/*` back
here); it defaults to `https://pay.sh`.

Privy login on the consent page needs, from dashboard.privy.io:
`PRIVY_APP_ID` and `PRIVY_APP_SECRET` (App settings),
`PRIVY_AUTHORIZATION_PRIVATE_KEY` (a `wallet-auth:…` P-256 private key) and
`PRIVY_SIGNER_ID` (the key quorum registered for its public key; the
dashboard's Authorization keys page creates both, or `POST /v1/key_quorums`
with a locally generated key). Token signatures are checked against the
app's JWKS, fetched once at startup from auth.privy.io (`PRIVY_JWKS_URL` to
override, or `PRIVY_VERIFICATION_KEY` for a single PEM). Optional:
`PRIVY_POLICY_ID` to attach a wallet policy to wallets pay-connect creates,
`PRIVY_API_BASE_URL` for a mock. The dashboard must also list the server's
origin under allowed origins. Put them in the repo-root `.env`; `dev/run.sh`
loads it.

## Frontend

The browser UI lives in the separate `solana-foundation/pay-web-ui`
repository and is deployed independently. Point local pay-connect at it with
`PAY_CONNECT_PAGES_URL`.

This crate does not build, embed, or serve frontend assets.

## Run

```sh
cargo run -p pay-connect -- --port 8402
```

Flags: `--bind` (default `127.0.0.1`), `--port` (default `8402`). Logging is
`RUST_LOG`-driven and goes to stderr.

Browser entrypoints redirect to `PAY_CONNECT_PAGES_URL`; unknown paths return
JSON 404 responses. `GET /health` returns `{"status":"ok"}`.

Try it end to end without the browser:

```sh
cargo run -p pay -- connect-onboard --url http://127.0.0.1:8402
```

## JSON endpoints

### `GET /v1/cli`

Browser entrypoint for `pay setup`. pay-connect validates the loopback
callback, state, and PKCE challenge, stores them server-side, then redirects
to `/connect?cli=<opaque request id>` in pay-web-ui. Callback and PKCE values
are not forwarded to the frontend.

### `GET /api/cli/{request}`

Returns the pending CLI view and Privy public login configuration. The
pay-web-ui proxy forwards the signed browser cookie when present.

### `POST /api/cli/{request}/approve`

Approves with a Privy access token or the server-authenticated browser
subject cookie. The Privy token is verified and its wallet is bound through
the same tenant path used by MCP. The response redirects to the validated
loopback callback with a short-lived one-time code and the original state.

`POST /api/cli/{request}/deny` consumes the request and redirects to the
loopback callback with `error=access_denied`.

### `POST /v1/cli/complete`

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
  "provider": "payconnect",
  "status": "ready",
  "network": "mainnet",
  "wallet_id": "wallet_test_not_a_real_id",
  "pubkey": "<Solana address>",
  "credentials": {
    "api_token": "<new tenant-scoped token>"
  }
}
```

The response never includes Privy app credentials or its authorization key.
The API token is stored in the platform secret store and used by the CLI's
`payconnect` remote provider.

### `GET /v1/wallets`

Lists the single wallet belonging to a CLI bearer token. The corresponding
`POST /v1/wallets/{id}/sign-transaction` and `sign-message` endpoints verify
that the requested wallet belongs to that token and sign through the
server-side Privy driver.
