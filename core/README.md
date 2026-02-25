# Solana Pay

`@solana/pay` is a JavaScript library for facilitating commerce on Solana by using a token transfer URL scheme. The URL scheme ensures that no matter the wallet or service used, the payment request must be created and interpreted in one standard way.

> **v1.0** — This version is built on [`@solana/kit`](https://github.com/anza-xyz/kit) v6. If you're migrating from v0.2 (which used `@solana/web3.js`), see the [Migration Guide](#migration-guide) below.

## Requirements

- Node.js >= 20 (required for Ed25519 crypto.subtle support)

## Installation

```bash
npm install @solana/pay @solana/kit
```

### Peer dependencies

| Package | Version |
|---------|---------|
| `@solana/kit` | `^6.1.0` |

### Optional dependencies (for `createTransfer` / `validateTransfer`)

| Package | Version |
|---------|---------|
| `@solana-program/system` | `^0.12.0` |
| `@solana-program/token` | `^0.11.0` |
| `@solana-program/token-2022` | `^0.9.0` |
| `@solana-program/memo` | `^0.11.0` |

## Quick Start

### Standalone usage

```ts
import { address } from '@solana/kit';
import { encodeURL, parseURL, createTransfer, findReference, validateTransfer } from '@solana/pay';

// 1. Create a payment URL
const recipient = address('MERCHANT_WALLET_ADDRESS');
const amount = 1; // 1 SOL
const url = encodeURL({ recipient, amount });

// 2. Show as QR code
import { createQR } from '@solana/pay';
const qr = createQR(url);

// 3. Create transfer instructions (caller composes the transaction)
import { createSolanaRpc } from '@solana/kit';
const rpc = createSolanaRpc('https://api.devnet.solana.com');
const instructions = await createTransfer(rpc, sender, { recipient, amount });

// 4. Find the transaction using a reference
const reference = address('UNIQUE_REFERENCE_ADDRESS');
const signatureInfo = await findReference(rpc, reference);

// 5. Validate the transfer
const response = await validateTransfer(rpc, signatureInfo.signature, { recipient, amount });
```

### Kit Plugin usage

Use the plugin to bind all Solana Pay methods to a kit client:

```ts
import { createEmptyClient } from '@solana/kit';
import { rpc, payerFromFile } from '@solana/kit-plugins';
import { solanaPay } from '@solana/pay';

const client = await createEmptyClient()
  .use(rpc('https://api.devnet.solana.com'))
  .use(payerFromFile('~/.config/solana/id.json'))
  .use(solanaPay());

// All pay methods are pre-bound to client.rpc and client.payer
const url = client.pay.encodeURL({ recipient, amount });
const instructions = await client.pay.createTransfer({ recipient, amount });
const parsed = client.pay.parseURL(url);
```

## API

### URL Encoding & Parsing

- **`encodeURL(fields)`** — Encode a transfer or transaction request into a `solana:` URL.
- **`parseURL(url)`** — Parse a `solana:` URL into its fields.

### QR Codes

- **`createQR(url, size?, background?, color?)`** — Create a QR code from a Solana Pay URL.

### Transfers

- **`createTransfer(rpc, sender, fields)`** — Create transfer `Instruction[]` for a payment. The caller composes the transaction using kit's `pipe()` pattern.
- **`findReference(rpc, reference, options?)`** — Find a transaction signature by reference address.
- **`validateTransfer(rpc, signature, fields, options?)`** — Validate that a confirmed transaction matches the expected payment.

### Transaction Requests

- **`fetchTransaction(rpc, account, link, options?)`** — Fetch a transaction from a transaction request endpoint.

### Plugin

- **`solanaPay()`** — Kit plugin that adds a `pay` namespace to any client with `rpc` (and optionally `payer`).

## How it works

### Web app to mobile wallet

Payment requests can be encoded as a URL according to the scheme, scanned using a QR code, sent and confirmed by the wallet, and discovered by the app.

### Web app to browser wallet

With a Solana Pay button, you could integrate an embeddable payment button that can be added to your existing app.

### Mobile app to mobile wallet

Payment requests could be encoded as a deep link. The app prepares a payment request, and passes control to the wallet. The wallet signs, sends, and confirms it, or cancels the request and passes control back to the app.

## Transaction Requests

A Solana Pay transaction request URL describes an interactive request for any Solana transaction. The parameters in the URL are used by a wallet to make an HTTP request to compose any transaction.

## Transfer Requests

A Solana Pay transfer request URL describes a non-interactive request for a SOL or SPL Token transfer. The parameters in the URL are used by a wallet to directly compose the transaction.

## Migration Guide

### v0.2 → v1.0

**Breaking changes:**

| v0.2 (`@solana/web3.js`) | v1.0 (`@solana/kit`) |
|--------------------------|----------------------|
| `PublicKey` | `Address` (branded string — use `address()` to create, `===` to compare) |
| `Connection` | `Rpc` from `@solana/kit` |
| `BigNumber` (from `bignumber.js`) | `number` — plain JS number for human-readable amounts |
| `createTransfer()` returns `Transaction` | Returns `Instruction[]` — compose with kit's `pipe()` + `createTransactionMessage()` |
| `sender: PublicKey` | `sender: TransactionSigner` |
| `@solana/spl-token` | `@solana-program/token` |
| `Buffer` | `Uint8Array` / `TextEncoder` |

**Typical migration:**

```diff
- import { Connection, PublicKey, Transaction } from '@solana/web3.js';
+ import { address, createSolanaRpc, pipe, createTransactionMessage,
+          setTransactionMessageFeePayer, appendTransactionMessageInstructions,
+          setTransactionMessageLifetimeUsingBlockhash, compileTransaction,
+          signTransaction, getBase64EncodedWireTransaction } from '@solana/kit';

- const connection = new Connection('https://api.devnet.solana.com');
+ const rpc = createSolanaRpc('https://api.devnet.solana.com');

- const recipient = new PublicKey('...');
+ const recipient = address('...');

- const transaction = await createTransfer(connection, sender.publicKey, { recipient, amount });
- transaction.feePayer = sender.publicKey;
- transaction.recentBlockhash = (await connection.getLatestBlockhash()).blockhash;
+ const instructions = await createTransfer(rpc, sender, { recipient, amount });
+ // Compose transaction using kit pipe() pattern
```

## License

The Solana Pay JavaScript SDK is open source and available under the Apache License, Version 2.0. See the [LICENSE](./LICENSE) file for more info.
