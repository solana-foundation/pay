# Example Server

A demo server with both MPP and x402 gated endpoints, powered by [Surfpool](https://github.com/txtx/surfpool).

## Prerequisites

- Node.js 20+

## Setup

```bash
pnpm install
pnpm dev      # watch mode — restarts on file changes
pnpm start    # single run
```

By default the server connects to the public Surfpool devnet at `402.surfnet.dev:8899`. No local setup needed.

## Using a local Surfpool instead

If you prefer to run a local Surfpool instance:

```bash
# Install and start Surfpool
curl -sL https://run.surfpool.run/ | bash
surfpool start

# Point the server at localhost
RPC_URL=http://localhost:8899 pnpm dev
```

Then use `--dev --local` with `pay`:

```bash
pay --dev --local curl http://localhost:3402/mpp/quote/SOL
```

## Endpoints

### MPP (www-authenticate header)

```bash
pay --dev curl http://localhost:3402/mpp/quote/AAPL
pay --dev curl http://localhost:3402/mpp/weather/paris
```

### x402 (X-PAYMENT-REQUIRED header)

```bash
pay --dev curl http://localhost:3402/x402/joke
pay --dev curl http://localhost:3402/x402/fact
```

### Free

```bash
curl http://localhost:3402/health
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RPC_URL` | `http://402.surfnet.dev:8899` | Surfpool RPC endpoint |
| `PORT` | `3402` | Server port |
| `NETWORK` | `localnet` | Solana network |
| `SECRET_KEY` | `test-secret-key-for-dev` | MPP secret key |
| `FEE_PAYER_KEY` | *(generated)* | Base58 keypair for fee payer |
| `RECIPIENT` | *(fee payer)* | Payment recipient address |

## How it works

- **MPP endpoints** use `@solana/mpp` with the `www-authenticate` / `Authorization` header flow
- **x402 endpoints** use `x402-express` with the `X-PAYMENT-REQUIRED` / `X-PAYMENT` header flow
- An **embedded local facilitator** runs on port 3403 to handle x402 verify/settle without needing an external service
- Both are configured to accept USDC payments with server-sponsored fees
- On startup, the server bootstraps the fee payer with 100 SOL + 1000 USDC via surfnet cheatcodes
