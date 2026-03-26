# Example Server

A demo server with both MPP and x402 gated endpoints, configured for localnet.

## Prerequisites

- [Surfpool](https://github.com/txtx/surfpool) running on `localhost:8899`
- Node.js 20+

## Setup

```bash
pnpm install
pnpm dev      # watch mode — restarts on file changes
pnpm start    # single run
```

## Endpoints

### MPP (www-authenticate header)

```bash
pay --yes curl http://localhost:3402/mpp/quote/AAPL
pay --yes curl http://localhost:3402/mpp/weather/paris
```

### x402 (X-PAYMENT-REQUIRED header)

```bash
pay --yes curl http://localhost:3402/x402/joke
pay --yes curl http://localhost:3402/x402/fact
```

### Free

```bash
curl http://localhost:3402/health
```

## How it works

- **MPP endpoints** use `@solana/mpp` with the `www-authenticate` / `Authorization` header flow
- **x402 endpoints** use `x402-express` with the `X-PAYMENT-REQUIRED` / `X-PAYMENT` header flow
- An **embedded local facilitator** runs on port 3403 to handle x402 verify/settle without needing an external service
- Both are configured to accept USDC payments on localnet with server-sponsored fees
