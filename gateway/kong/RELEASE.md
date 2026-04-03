# Release And Installation

This document covers the operator-facing release flow for the native Lua Kong plugin `solana-kong-402`.

## What must be shipped

Every usable release needs these artifacts:

1. the Lua plugin package source in `gateway/kong`
2. the Kong schema file at `kong/plugins/solana-kong-402/schema.lua`
3. the bundled `mpp` Lua runtime included in the package
4. LuaRocks packaging metadata in the repo-root `pay-0.1.0-1.rockspec`
5. installation instructions for self-managed Kong, Kubernetes, and Konnect

## Build artifacts

Build and validate the package locally:

```bash
cd ../..
luarocks make pay-0.1.0-1.rockspec
luarocks pack pay-0.1.0-1.rockspec
```

The schema file is:

```bash
kong/plugins/solana-kong-402/schema.lua
```

## Self-managed Kong

On every Kong node, either install the rock or bake the package into the image.

LuaRocks install:

```bash
luarocks install pay-0.1.0-1.all.rock
```

Then configure Kong:

```conf
plugins = bundled,solana-kong-402
```

Restart or reload Kong, then apply route or service configuration with decK or the Admin API.

Example decK state:

- `examples/deck/kong.yaml`

## Kubernetes

Kubernetes operators typically build a custom Kong image that already contains:

- `kong/plugins/solana-kong-402`
- `mpp/`

Artifacts in this repository:

- Helm values overlay: `examples/k8s/values.yaml`
- schema ConfigMap example: `examples/k8s/schema-configmap.yaml`
- plugin config: `examples/k8s/kongplugin.yaml`
- route attachment: `examples/k8s/httproute.yaml`
- secret example: `examples/k8s/secret.yaml`

## Konnect

Konnect requires the schema to be uploaded to the control plane, while the plugin package still has to exist on each data plane node.

See:

- `examples/konnect/README.md`

## Versioning

Recommended release structure:

- tag the repository with a plugin version such as `v0.1.0`
- publish the LuaRocks package as `solana/pay`
- publish a container image for Kong deployments

## Compatibility notes

- current local dev image uses `kong:3.9.1`
- plugin scope is HTTP only
- route and service scoping are supported
- successful verification depends on the configured recipient and RPC endpoint living on the same Solana network
