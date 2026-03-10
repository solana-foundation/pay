/**
 * Solana Pay — Two-party payment flow
 *
 * Demonstrates the merchant/wallet split using role-specific clients:
 *
 * Merchant (no signer needed):
 *   1. Encodes a payment URL with recipient, amount, and reference
 *   2. Finds the transaction by reference after payment
 *   3. Validates the transfer matches expectations
 *
 * Wallet (signer required):
 *   1. Parses the payment URL
 *   2. Creates transfer instructions
 *   3. Sends the transaction (planner/executor handles signing + blockhash)
 *
 * Requirements:
 *   - @solana/kit, @solana/kit-plugins, @solana/pay
 *   - A running Solana validator (devnet or local)
 */

import { address, generateKeyPairSigner } from '@solana/kit';
import type { TransferRequestURL } from '../src/index.js';
import { createMerchantClient, createWalletClient } from '../src/index.js';

// ── Merchant side ──────────────────────────────────────────────────

const merchant = createMerchantClient({
    rpcUrl: 'https://api.devnet.solana.com',
});

const referenceSigner = await generateKeyPairSigner();
const reference = referenceSigner.address;

const recipient = address('mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN');
const amount = 0.01;

// 1. Merchant encodes the payment URL
const url = merchant.pay.encodeURL({
    recipient,
    amount,
    reference,
    label: 'Michael',
    message: 'Thanks for all the fish',
    memo: 'OrderId5678',
});
console.log('Payment URL:', url.toString());

// ── Wallet side ────────────────────────────────────────────────────

const walletSigner = await generateKeyPairSigner();
// Note: In a real app, fund the wallet first via airdrop or other means

const wallet = createWalletClient({
    rpcUrl: 'https://api.devnet.solana.com',
    payer: walletSigner,
});

// 2. Wallet parses the URL
const parsed = wallet.pay.parseURL(url) as TransferRequestURL;
console.log('Parsed recipient:', parsed.recipient);
console.log('Parsed amount:', parsed.amount?.toString());

// 3. Wallet creates transfer instructions and sends
const instructions = await wallet.pay.createTransfer({
    recipient: parsed.recipient,
    amount: parsed.amount!,
    splToken: parsed.splToken,
    reference: parsed.reference,
    memo: parsed.memo,
});

// 4. Wallet sends — planner/executor handles blockhash, signing, and submission
const result = await wallet.sendTransaction(instructions);
console.log('Transaction sent:', result);

// ── Back to merchant ───────────────────────────────────────────────

// 5. Merchant finds the transaction by reference
const found = await merchant.pay.findReference(reference);
console.log('Found signature:', found.signature);

// 6. Merchant validates the transfer
await merchant.pay.validateTransfer(found.signature, {
    recipient,
    amount,
    reference,
    memo: 'OrderId5678',
});
console.log('Transfer validated!');
