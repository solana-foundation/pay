# Agent Attestation Flow

Step-by-step guide for running a CAAP/1.0 agent attestation through the relay.

## Prerequisites

- A Solana wallet (keypair or browser extension)
- A Clerk session token from `relaxing-collie-65.accounts.dev` (optional, for Clerk-bridged flows)
- The relay URL: `https://relay.clawd.xyz`

## Flow 1: SIWS-Only (No Clerk)

Use this when you have a Solana wallet and want direct attestation without Clerk.

### Step 1: Get SIWS Challenge

```
POST https://relay.clawd.xyz/api/siws/challenge
Content-Type: application/json

{
  "walletAddress": "<your-solana-address>"
}
```

Returns a SIWS input with `{ domain, address, statement, uri, version, chainId, nonce, issuedAt }`.

### Step 2: Sign the SIWS Message

Construct the SIWS message from the input fields and sign with the wallet's private key (Ed25519).

```ts
import { createSIWSMessage } from "better-auth-solana/client";

const message = createSIWSMessage({
  address: wallet.publicKey.toBase58(),
  domain: "relay.clawd.xyz",
  nonce: siwsInput.nonce,
  statement: "Sign in to Clawd Agent Auth",
});

const signature = await wallet.signMessage(new TextEncoder().encode(message));
```

### Step 3: Verify SIWS

```
POST https://relay.clawd.xyz/api/siws/verify
Content-Type: application/json

{
  "message": "<siws-message>",
  "signature": "<base64-ed25519-signature>",
  "walletAddress": "<your-solana-address>",
  "agentId": "<optional-agent-id>"
}
```

Returns `{ verified, attestation?, tee?, tier? }`.

## Flow 2: Full CAAP Attestation (With Clerk)

Use this when the agent already has a Clerk session from `relaxing-collie-65.accounts.dev`.

### Step 1: Get Clerk Session Token

```ts
import { useAuth } from "@clerk/nextjs";

const { getToken } = useAuth();
const token = await getToken({ template: "solana_wallet" });
```

The JWT template must include `wallet_address` and `agent_id` from `user.publicMetadata`.

### Step 2: Run Attestation

```
POST https://relay.clawd.xyz/api/caap/attest
Authorization: Bearer <clerk-session-token>
Content-Type: application/json

{
  "walletAddress": "<your-solana-address>"
}
```

Returns:
```json
{
  "verified": true,
  "attestation": {
    "agentId": "my-agent",
    "wallet": "<solana-address>",
    "clawdMint": "8cHzQHUS2s2h8TzCmfqPKYiM4dSt4roa3n7MyRLApump",
    "attestationHash": "sha256-hex...",
    "tokenBalance": 1500000,
    "agentNftAddress": "<nft-mint>"
  },
  "tee": {
    "intelQuote": "<base64-tdx-quote>",
    "explorerUrl": "https://proof.t16z.com/?attestation=...",
    "mrAggregated": "<hex>",
    "mrtd": "<hex>",
    "rtmr0": "<hex>",
    "hasTeeEvidence": true
  },
  "tier": {
    "tier": "gold",
    "clawdRequired": 1000000,
    "clawdToNextTier": 3500000,
    "percentToNext": 70
  }
}
```

## Flow 3: Lightweight Status Check

For quick verified/unverified checks without running the full TEE attestation:

```
GET https://relay.clawd.xyz/api/caap/status/:agentId
```

Returns `{ verified: boolean }`. Use this before paying for a full attestation.

## Flow 4: TEE Health Check

Verify the relay's TEE instance is healthy and get a fresh quote:

```
GET https://relay.clawd.xyz/api/tee/report
```

Returns a fresh Intel TDX quote bound to the current nonce, verifying that the relay is still running inside a genuine Phala TEE CVM.

## Billing

All attestation endpoints bill per-request via the x402 payment protocol. The relay returns HTTP 402 Payment Required with an x402 challenge. Pay handles the payment flow transparently.

- `/api/caap/status/:agentId` — lowest cost, no TEE quote
- `/api/caap/attest` — full attestation, highest cost
- `/api/siws/verify` — medium cost (SIWS + optional TEE)
- `/api/tee/report` — fresh TEE quote only

## Error Handling

| Status | Meaning |
|--------|---------|
| 200 | Success with attestation payload |
| 400 | Missing or invalid wallet address / SIWS signature |
| 401 | Missing or invalid Clerk session token |
| 402 | Payment required — use `pay curl` to handle automatically |
| 404 | Agent ID not found (status endpoint) |
| 500 | Relay internal error or TEE quote generation failure |