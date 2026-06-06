# Subscription Tiers And Token Gating

How CLAWD token balances map to subscription tiers and unlock agent auth features.

## Tier Thresholds

| Tier | CLAWD Required | Cumulative |
|------|---------------|------------|
| Free | 0 | 0 |
| Bronze | 100,000 | 100,000 |
| Silver | 500,000 | 500,000 |
| Gold | 1,000,000 | 1,000,000 |
| Diamond | 5,000,000 | 5,000,000 |

## Tier Features

### Free (0 CLAWD)
- Basic SIWS sign-in
- Discovery endpoint access
- Cannot run full attestation (status checks only)

### Bronze (100,000 CLAWD)
- Everything in Free
- Full CAAP attestation (SIWS + DAS + token attestation)
- Peer card generation
- Basic agent profile in directory

### Silver (500,000 CLAWD)
- Everything in Bronze
- Priority verification queue
- Verification history (last 90 days)
- Multi-agent management (up to 5 agents per wallet)

### Gold (1,000,000 CLAWD)
- Everything in Silver
- Real-time wallet monitoring
- Webhook notifications for attestation events
- Team accounts (up to 10 members)
- API rate limit increase

### Diamond (5,000,000 CLAWD)
- Everything in Gold
- Dedicated verification node
- Enterprise SLA (99.9% uptime)
- White-label attestation domain
- Custom tier branding
- Priority support

## Computing Tiers

### From the SDK

```ts
import { computeTier, TIER_THRESHOLDS } from "@clawd/agent-auth-solana";

const tier = computeTier(clawdBalance);

// tier: {
//   tier: "gold",           // Tier label
//   clawdRequired: 1000000,  // CLAWD needed for this tier
//   nextTier?: {             // Next tier info (undefined for Diamond)
//     tier: "diamond",
//     clawdRequired: 5000000,
//   },
//   clawdToNextTier?: 4000000, // CLAWD needed to reach next tier (undefined for Diamond)
//   percentToNext?: 80,        // 0-100 progress to next tier (undefined for Diamond)
// }
```

### From the Relay API

The `/api/caap/attest` response includes a `tier` field:

```json
{
  "tier": {
    "tier": "gold",
    "clawdRequired": 1000000,
    "clawdToNextTier": 3500000,
    "percentToNext": 70
  }
}
```

## Token Balance Sources

The relay checks CLAWD balance from two sources:

1. **Primary**: SPL token account on the attestation wallet
2. **Fallback**: Delegated token accounts (for multi-wallet setups)

Use `fetchWalletSnapshot` for a full balance picture:

```ts
import { fetchWalletSnapshot } from "@clawd/agent-auth-solana";

const snapshot = await fetchWalletSnapshot(walletAddress, {
  heliusRpcUrl: `https://mainnet.helius-rpc.com/?api-key=${HELIUS_API_KEY}`,
});

// snapshot: {
//   walletAddress: string,
//   solBalance: number,       // SOL in lamports
//   clawdBalance: number,    // CLAWD raw amount
//   tokenAccounts: Array<{ mint, balance, decimals }>,
//   fetchedAt: number,       // Unix timestamp
// }
```

## CLAWD Token Details

- **Mint Address**: `8cHzQHUS2s2h8TzCmfqPKYiM4dSt4roa3n7MyRLApump`
- **Network**: Solana mainnet-beta
- **Program**: Token (TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA)
- **Decimals**: 6

## Tier Gating In Better Auth

When using `createCaapPlugin` with `enableSubscriptionTiers: true`, endpoints are automatically gated:

```ts
import { createCaapPlugin } from "@clawd/agent-auth-solana";

createCaapPlugin({
  heliusApiKey: process.env.HELIUS_API_KEY,
  clawdMint: "8cHzQHUS2s2h8TzCmfqPKYiM4dSt4roa3n7MyRLApump",
  enableSubscriptionTiers: true,   // Enables automatic tier gating
  enableDasAttestation: true,      // Enables DAS NFT verification
});
```

The plugin enforces:
- Free tier: `/caap/discovery` only
- Bronze+: `/caap/attest`, `/caap/status`
- Silver+: Rate-limited access with history
- Gold+: Webhook registration, team endpoints
- Diamond+: All endpoints with elevated limits

## Upgrading Tiers

Tiers are computed dynamically from the on-chain CLAWD balance — no manual upgrade flow needed. Buy CLAWD on any Solana DEX and the next attestation will reflect the new tier.

No SOL is needed for attestation — the relay's server-side fee payer handles transaction costs.