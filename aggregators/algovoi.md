---
name: algovoi
title: AlgoVoi multi-protocol facilitator
url: https://algovoi.co.uk
contact: chopmob@gmail.com
description: Multi-chain x402 + MPP + AP2 facilitator covering seven mainnet networks (Algorand, VOI, Hedera, Stellar, Base, Solana, Tempo). Routes Solana via Circle CCTP V2 (~50s, native USDC) and Algorand + Stellar via Allbridge Core. Stellar destination went live 2026-05-05; first mainnet bridge from Base settled in ~2 minutes to canonical Circle USDC at the merchant's classic Stellar address.
catalog_url: https://api.algovoi.co.uk/.well-known/pay-skills.json
---

# AlgoVoi

AlgoVoi is a hosted x402 facilitator that lets API providers and AI agents accept stablecoin payments without managing on-chain infrastructure or destination-chain wallets directly. The same gateway speaks the [x402 Payment Protocol](https://x402.org), [Machine Payments Protocol (MPP)](https://github.com/circle-platform/mpp), and [Agent Payments Protocol (AP2)](https://github.com/google-agentic-commerce/AP2) — providers pick whichever protocol matches their client, and AlgoVoi handles verification, settlement, and webhooks.

## What this aggregator entry exposes

Live AlgoVoi tenants opt their `resource_definitions` into the public catalog by setting `is_active=true` on the resource and `mode=live, status=active` on the tenant. The aggregator endpoint at `catalog_url` returns those entries in the pay.sh `version: 2` envelope shape — `provider_count`, `providers[]`, strong `ETag`, `Cache-Control: public, max-age=60`, and conditional `If-None-Match` support so `pay skills sync` can poll cheaply.

Each provider entry includes:

- `fqn` — `algovoi/<tenant_short_id>/<resource_id>` triple-namespaced for stable identity across tenants.
- `service_url` — public AlgoVoi-hosted resource path (`https://api.algovoi.co.uk/r/<tenant>/<resource>`).
- `min_price_usd` / `max_price_usd` — derived from `amount_microunits` for USDC across all three encoding formats (Algorand ASA `31566704`, Solana mint `EPjFWdd5…`, Stellar classic issuer `GA5ZSEJ…` / 7dp scaling).
- `category` — heuristic over the resource slug (`agent`, `rpc`, `data`, `messaging`, fallback `payment-gated-resource`).
- `sha` — deterministic 16-char content fingerprint, stable across regenerations when nothing has changed.

Test-mode tenants and any non-mainnet networks are filtered out before serialisation; the public catalog is production-grade only.

## xChain — pay from MetaMask, settle on the destination chain

Every AlgoVoi-protected resource on Algorand, Solana, or Stellar mainnet exposes a MetaMask payment path. Customers holding USDC on any of seven supported EVM source chains (Base, Arbitrum, Polygon, Optimism, Ethereum, BNB Chain, Avalanche) can pay through pay.sh's CLI without ever installing a destination-chain wallet — AlgoVoi bridges the USDC end-to-end and verifies delivery before unlocking the resource.

- **Solana** — Circle CCTP V2 Fast Transfer (default). ~50s end-to-end, zero pool slippage, native Circle USDC on both sides.
- **Algorand** — Allbridge Core delivers Allbridge-wrapped USDCa (ASA `31566704`) to a deterministically-derived LogicSig address, then settles native to the merchant.
- **Stellar** (live since 2026-05-05) — Allbridge Core's swap-and-bridge route invokes a Soroban swap pool that auto-converts to canonical Circle USDC at the classic issuer (`GA5ZSEJ…`) and credits the merchant's `G…` address via a normal `account_credited` effect. **Any Stellar wallet supports the asset** — no Soroban-aware wallet required.

CCTP V2 will be added as a second protocol on Stellar once Circle ships Stellar mainnet contracts publicly (currently testnet-only).

## Wallet expectations

AlgoVoi is non-custodial — merchants hold their own destination-chain keys. For pay.sh users paying AlgoVoi-protected endpoints, the local biometric signing flow (Touch ID / Windows Hello / GNOME Keyring) is the recommended pattern; AlgoVoi never sees the customer's private key.

## Stable contact + canonical URLs

- Catalog: <https://api.algovoi.co.uk/.well-known/pay-skills.json>
- Discovery (a2a / x402): <https://api.algovoi.co.uk/.well-known/agent-card.json>
- Security policy: <https://api.algovoi.co.uk/.well-known/security.txt>
- Documentation: <https://docs.algovoi.co.uk/concepts/xchain>
- Operator email: <chopmob@gmail.com>
