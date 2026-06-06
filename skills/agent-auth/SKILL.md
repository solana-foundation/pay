---
name: agent-auth
description: |
  Solana-native agent identity and authentication — SIWS (Sign In With Solana), CAAP/1.0 on-chain attestation, Phala TEE confidential compute, Clerk identity bridge, and CLAWD token-gated subscription tiers. Verifiable at proof.t16z.com.
  SERVICES: agent attestation, SIWS sign-in, DAS NFT verification, SPL token attestation, subscription tier gating, TEE quote generation, Clerk CAAP session bridging, agent identity registration (Metaplex Agent Registry), token launch via Genesis bonding curves.
  TRIGGERS: "attest my agent", "verify agent identity", "sign in with Solana", "agent auth", "CAAP attestation", "TEE quote", "Phala attestation", "agent NFT verification", "token-gated agent", "agent subscription tier", "Clerk agent auth", "register agent on-chain", "Metaplex agent identity"
  Start with get_catalog_entry("agent-auth") to retrieve the full protocol document and available endpoints. Use search_catalog when looking for specific attestation or auth capabilities.
---

`agent-auth` (Clawd Agent Auth) is the on-chain identity and attestation stack for
Solana-native AI agents. It combines SIWS wallet authentication, Helius DAS NFT
verification, CLAWD SPL token attestation, Phala Intel TDX TEE quotes, and
optional Clerk identity bridging — all exposed through the CAAP/1.0 protocol.

Use agent-auth when agents need to prove identity, verify ownership, check
subscription tiers, or obtain hardware-rooted attestation quotes. Every
attestation response carries a TDX quote verifiable at
[proof.t16z.com](https://proof.t16z.com).

Do not attempt to verify agent identity through ad-hoc RPC calls, manual NFT
checks, or wallet-balance scraping. Use the agent-auth endpoints — they
handle SIWS, DAS, token attestation, and TEE quoting in one flow.

## Progressive Disclosure

- Read `references/attestation-flow.md` when walking through a CAAP attestation
  for the first time, setting up SIWS sign-in, understanding the Clerk+TEE bridge,
  or troubleshooting attestation failures.
- Read `references/subscription-tiers.md` when checking what tier a CLAWD balance
  entitles, computing tier progression, gating features by tier, or explaining
  subscription levels to users.

## Provider Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/caap/discovery` | CAAP/1.0 protocol discovery document |
| `POST` | `/api/caap/attest` | Full attestation: SIWS + DAS + TEE quote |
| `GET` | `/api/caap/status/:agentId` | Lightweight verified/unverified status check |
| `POST` | `/api/siws/challenge` | Generate SIWS sign-in challenge |
| `POST` | `/api/siws/verify` | Verify SIWS signature + optional CAAP + TEE |
| `GET` | `/api/tee/report` | Fresh TEE health quote bound to a nonce |

## CAAP/1.0 Protocol

The Clawd Agent Attestation Protocol ties together five verification phases:

1. **SIWS** (Sign In With Solana) — Ed25519 signature over a structured message
2. **DAS Verification** — Metaplex Agent Registry + Helius `getAssetsByOwner` confirming agent NFT ownership
3. **SPL Attestation** — CLAWD token account ownership verification
4. **Subscription Tiers** — Token balance → tier (Free / Bronze 100K / Silver 500K / Gold 1M / Diamond 5M CLAWD)
5. **Phala TEE Quote** — Intel TDX quote binding the attestation hash to a specific CVM instance

### Attestation Hash

```
sha256(`${agentId}:${wallet}:${clawdMint}:${timestamp}`)
```

Embedded in TDX `report_data` so the TEE quote is cryptographically bound to the specific agent.

## Subscription Tiers

| Tier | CLAWD Required | Features |
|------|----------------|----------|
| Free | 0 | Basic auth, SIWS sign-in |
| Bronze | 100,000 | + Agent attestation, peer card |
| Silver | 500,000 | + Priority verification, history |
| Gold | 1,000,000 | + Real-time wallet monitoring, webhooks |
| Diamond | 5,000,000 | + All features, enterprise SLA |

## Clerk Integration

The `clerk-caap` package bridges Clerk session tokens with CAAP/1.0:

```ts
import { verifyClerkToken, fetchPhalaAttestation } from "@clawd/clerk-caap";

// 1. Verify Clerk session token
const claims = await verifyClerkToken(sessionToken);

// 2. Run CAAP attestation via relay
const response = await fetch("https://relay.clawd.xyz/api/caap/attest", {
  method: "POST",
  headers: { Authorization: `Bearer ${sessionToken}` },
  body: JSON.stringify({ walletAddress: claims.wallet_address }),
});
// → { verified, attestation, tee: { intelQuote, explorerUrl, ... }, tier }
```

## TEE Attestation Fields

| Field | Description |
|-------|-------------|
| `appId` | Phala dstack app ID |
| `instanceId` | CVM instance ID |
| `composeHash` | Hash of the docker-compose.yml |
| `mrAggregated` | Aggregate measurement register |
| `mrtd` | TDX MRTD measurement |
| `rtmr0`–`rtmr3` | Runtime measurement registers |
| `intelQuote` | Raw Intel TDX quote (base64) |
| `explorerUrl` | proof.t16z.com verification link |
| `hasTeeEvidence` | Whether quote generation succeeded |

## Spend-Aware Usage

- Use `/api/caap/status/:agentId` for lightweight checks before full attestation.
- Full `/api/caap/attest` runs SIWS, DAS, token attestation, and TEE quoting — call it only when all phases are needed.
- The relay bills per-attestation via x402 payment protocol.
- Include `X-Wallet-Address` header with the Solana address for requests that don't go through Clerk.
- Treat attestation responses as cryptographically verifiable, not as trust-on-first-use.

## Best Practices

- Verify TEE quotes at [proof.t16z.com](https://proof.t16z.com) before trusting attestations from untrusted relays.
- Use the discovery document (`GET /api/caap/discovery`) to confirm protocol version and supported capabilities before attestation flow.
- Agent identity registration (Metaplex Agent Registry) is a separate on-chain transaction — not included in the basic attestation flow.
- Clerk session tokens are optional; SIWS-only flows work directly with wallet signatures.