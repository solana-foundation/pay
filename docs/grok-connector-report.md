# Grok custom connector: OAuth stops after dynamic client registration

Report for xAI, prepared 2026-09-17 from server-side request logs.

## Summary

Adding a remote MCP server that requires OAuth 2.1 as a **Custom** connector
on grok.com fails with "Sorry, there was a problem connecting to this MCP
server" (or "this connector is unavailable at the moment"). The server sees
Grok complete discovery and dynamic client registration successfully and
receive a `201 Created`, after which Grok makes **no further request**: the
user's browser is never sent to the authorization endpoint. The same server
completes the flow with other MCP hosts and with a scripted RFC-conformant
client.

## Environment

- Server: pay-cloud (Rust, axum, rmcp 1.8 streamable HTTP), served over
  HTTPS through a public tunnel. Host header and TLS are valid.
- MCP transport: streamable HTTP at `/mcp`, stateful sessions.
- Authorization server: same origin, RFC 8414 + RFC 9728 metadata, RFC 7591
  registration, PKCE S256 required, opaque bearer tokens.
- Grok: grok.com web, Connectors → New Connector → Custom, server URL
  `https://<host>/mcp`, no other fields filled.

## What Grok sends (observed, in order, one attempt)

```
POST /mcp                                        -> 401  WWW-Authenticate: Bearer error="invalid_token",
                                                         error_description="…", resource_metadata="https://<host>/.well-known/oauth-protected-resource", scope="mcp"
POST /mcp                                        -> 401
GET  /.well-known/oauth-protected-resource       -> 200
GET  /.well-known/oauth-authorization-server     -> 200
POST /mcp                                        -> 401
GET  /mcp                                        -> 401
GET  /.well-known/oauth-protected-resource       -> 200
GET  /.well-known/oauth-authorization-server     -> 200
POST /oauth/register                             -> 201
(nothing further; Grok shows the error)
```

Registration request body, verbatim:

```json
{"client_name":"Grok",
 "redirect_uris":["https://grok.com/connectors-oauth-exchange-code/"],
 "grant_types":["authorization_code","refresh_token"],
 "response_types":["code"],
 "token_endpoint_auth_method":"none"}
```

Registration response (201, `application/json`):

```json
{"client_id":"cli_…",
 "client_name":"Grok",
 "redirect_uris":["https://grok.com/connectors-oauth-exchange-code/"],
 "grant_types":["authorization_code","refresh_token"],
 "response_types":["code"],
 "token_endpoint_auth_method":"none",
 "client_id_issued_at":1789678099}
```

Protected resource metadata:

```json
{"resource":"https://<host>/mcp",
 "authorization_servers":["https://<host>"],
 "bearer_methods_supported":["header"],
 "scopes_supported":["mcp"],
 "resource_name":"Pay"}
```

Authorization server metadata (abridged):

```json
{"issuer":"https://<host>",
 "authorization_endpoint":"https://<host>/oauth/authorize",
 "token_endpoint":"https://<host>/oauth/token",
 "registration_endpoint":"https://<host>/oauth/register",
 "revocation_endpoint":"https://<host>/oauth/revoke",
 "response_types_supported":["code"],
 "response_modes_supported":["query"],
 "grant_types_supported":["authorization_code","refresh_token"],
 "code_challenge_methods_supported":["S256"],
 "token_endpoint_auth_methods_supported":["none","client_secret_post","client_secret_basic"],
 "scopes_supported":["mcp"]}
```

The same documents are also served at the path-based locations
(`/.well-known/oauth-protected-resource/mcp`,
`/.well-known/oauth-authorization-server/mcp`) and at
`/.well-known/openid-configuration`. CORS allows any origin on all of them.

## What we tried

- Registering as `none`, `client_secret_post` and `client_secret_basic`
  clients (the server accepts all three): same result.
- Full RFC 6750 challenge with `error_description` and `scope`: same result.
- Removing and re-adding the connector, new hostnames: same result.
- A scripted client that performs exactly Grok's sequence and then continues
  to `/oauth/authorize` with PKCE completes the flow and obtains a working
  token; the browser consent page is reachable.
- The connector dialog offers no place to enter a bearer header; requests
  from Grok carry no `Authorization` header. With authentication switched
  off entirely (a development mode), Grok connects and lists tools, so the
  transport and the tunnel are fine.

## Questions

1. After a successful `POST /oauth/register`, what does Grok require before
   it opens the authorization endpoint? Is any field expected in the 201
   body beyond RFC 7591 (for example `scope`, `client_secret` for public
   clients, or a specific `client_id_issued_at` form)?
2. Does Grok reject authorization servers on certain hostnames (tunnel
   domains), or require the `resource` to match the entered URL in a
   particular normalisation (trailing slash, path-based metadata only)?
3. Is `https://grok.com/connectors-oauth-exchange-code/` the redirect URI
   servers should allowlist? Public documentation mentions
   `https://grok.com/connectors/oauth/callback`.
4. Is there any client-side log or error code the user can retrieve when a
   Custom connector fails, so server operators can diagnose without
   guessing?

## Contact

Ludo Galabru, Solana Foundation. Server source: github.com/solana-foundation/pay
(`rust/crates/cloud`).
