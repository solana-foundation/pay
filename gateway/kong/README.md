# solana-kong-402

`gateway/kong` is the native Lua Kong plugin package.

It is intentionally self-contained:

- Kong plugin code lives under `kong/plugins/solana-kong-402`
- the bundled Lua MPP runtime lives under `mpp/`
- the package is published from the repo-root `pay-0.1.0-1.rockspec`

## What it does

- returns `402 Payment Required` with an MPP `WWW-Authenticate` challenge
- verifies `Authorization: Payment ...` credentials natively from Kong over Solana JSON-RPC
- supports both `type="transaction"` and `type="signature"` credentials
- propagates `Payment-Receipt` downstream
- injects `X-MPP-*` receipt metadata upstream

## Package layout

- `kong/plugins/solana-kong-402/handler.lua`
- `kong/plugins/solana-kong-402/schema.lua`
- `mpp/`
- `../../pay-0.1.0-1.rockspec`

## Local dev

From the repo root:

```bash
just kong dev-start
curl -i http://127.0.0.1:8000/paid
pay --sandbox --yolo --verbose curl -i http://127.0.0.1:8000/paid
```

Useful commands:

- `just kong dev-up`
- `just kong dev-start`
- `just kong dev-down`
- `just kong dev-logs`

The dev stack is defined in:

- `dev/docker-compose.yml`
- `dev/kong.conf`
- `dev/kong.yml`

## Distribution

Build and install with LuaRocks:

```bash
cd ../..
luarocks make pay-0.1.0-1.rockspec
```

This rock is intended to be published as `solana/pay`.

That installs:

- `kong.plugins.solana-kong-402.handler`
- `kong.plugins.solana-kong-402.schema`
- bundled `mpp.*` Lua modules

Then enable it in Kong:

```conf
plugins = bundled,solana-kong-402
```

For containerized deployments, the sample image is:

- `Dockerfile`

Example publish flow:

```bash
cd ../..
luarocks pack pay-0.1.0-1.rockspec
luarocks upload --api-key "$LUAROCKS_API_KEY" pay-0.1.0-1.all.rock
```

## Examples

Examples in this directory now target the Lua plugin:

- decK: `examples/deck/kong.yaml`
- KIC: `examples/k8s/kongplugin.yaml`
- Konnect notes: `examples/konnect/README.md`

## Current limitation

Replay protection is still per Kong worker process, not shared across workers or nodes.
