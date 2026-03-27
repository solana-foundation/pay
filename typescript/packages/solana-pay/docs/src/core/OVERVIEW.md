---
title: Overview
slug: /core/overview
---

# Solana Pay

`@solana/pay` is a JavaScript library for facilitating commerce on Solana by using a token transfer URL scheme. The URL scheme ensures that no matter the wallet or service used, the payment request must be created and interpreted in one standard way.

## Why use Solana Pay

Businesses and developers can use Solana Pay to accept payments in SOL or any SPL token without intermediaries. It offers frictionless and portable integration options like payment links, pay now buttons or QR codes on your app, dApp, website, blog, and so much more.

## Installation

```shell
pnpm add @solana/pay
```

## Quick Start

The library provides role-specific clients so each side of a payment only has access to what it needs.

**Merchant** (no signer needed) — encode payment URLs, generate QR codes, find and validate payments:

```typescript
import { address } from '@solana/kit';
import { createMerchantClient } from '@solana/pay';

const merchant = createMerchantClient({
    rpcUrl: 'https://api.mainnet-beta.solana.com',
});

const url = merchant.pay.encodeURL({
    recipient: address('MERCHANT_WALLET'),
    amount: 1.5,
    label: 'My Store',
});

const qrCode = merchant.pay.createQR(url);
```

**Wallet** (signer required) — parse URLs, create transfers, send transactions:

```typescript
import { createWalletClient } from '@solana/pay';

const wallet = createWalletClient({
    rpcUrl: 'https://api.mainnet-beta.solana.com',
    payer: walletSigner,
});

const parsed = wallet.pay.parseURL(url);
const instructions = await wallet.pay.createTransfer({ recipient, amount });
await wallet.sendTransaction(instructions);
```

**Combined** — if you need both merchant and wallet methods in one client:

```typescript
import { createSolanaPayClient } from '@solana/pay';

const client = createSolanaPayClient({
    rpcUrl: 'https://api.mainnet-beta.solana.com',
    payer: walletSigner,
});
```

## Getting Started

Learn how to integrate Solana Pay in your website, application or wallet.

### Transaction Requests

A Solana Pay transaction request URL describes an interactive request for any Solana transaction. The parameters in the URL are used by a wallet to make an HTTP request to compose any transaction.

- [Create a transaction request](./transaction-request/MERCHANT_INTEGRATION.md)

### Transfer Requests

A Solana Pay transfer request URL describes a non-interactive request for a SOL or SPL Token transfer. The parameters in the URL are used by a wallet to directly compose the transaction.

- [Create a transfer request](./transfer-request/MERCHANT_INTEGRATION.md)
- [Handle a transfer request](./transfer-request/WALLET_INTEGRATION.md)

## Other resources

- [API Reference](https://docs.solanapay.com/api/core)
- [Brand Guidelines](https://solanapay.com/branding)
- [Solana Pay Buttons](https://www.figma.com/community/file/1070341985720702755)

## License

The Solana Pay JavaScript SDK is open source and available under the Apache License, Version 2.0.
