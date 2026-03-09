/**
 * Solana Pay — Example payment flow
 *
 * This example demonstrates the full lifecycle of a Solana Pay transfer request:
 * 1. Merchant encodes a payment URL with recipient, amount, and reference
 * 2. Wallet parses the URL and creates transfer instructions
 * 3. Wallet signs and sends the transaction
 * 4. Merchant finds and validates the payment using the reference
 *
 * Requirements:
 *   - @solana/kit, @solana/kit-plugins, @solana/pay
 *   - A running Solana validator (devnet or local)
 */

import {
    address,
    createSolanaRpc,
    generateKeyPairSigner,
    pipe,
    createTransactionMessage,
    setTransactionMessageFeePayer,
    setTransactionMessageLifetimeUsingBlockhash,
    appendTransactionMessageInstructions,
    signTransaction,
    getBase64EncodedWireTransaction,
    compileTransaction,
} from '@solana/kit';
import type { TransferRequestURL } from '../src/index.js';
import { createTransfer, encodeURL, findReference, parseURL, validateTransfer } from '../src/index.js';

(async function () {
    const rpc = createSolanaRpc('https://api.devnet.solana.com');

    // Merchant app generates a random reference address to locate the transaction later
    const referenceSigner = await generateKeyPairSigner();
    const originalReference = referenceSigner.address;

    const recipient = address('mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN');
    const amount = 0.01;

    // 1. Encode the payment URL
    const url = encodeURL({
        recipient,
        amount,
        reference: originalReference,
        label: 'Michael',
        message: 'Thanks for all the fish',
        memo: 'OrderId5678',
    });
    console.log('Payment URL:', url.toString());

    // 2. Wallet parses the URL
    const parsed = parseURL(url) as TransferRequestURL;
    console.log('Parsed recipient:', parsed.recipient);
    console.log('Parsed amount:', parsed.amount?.toString());

    // 3. Wallet creates transfer instructions
    const wallet = await generateKeyPairSigner();
    // Note: In a real app, fund the wallet first via airdrop or other means

    const instructions = await createTransfer(rpc, wallet, {
        recipient: parsed.recipient,
        amount: parsed.amount!,
        splToken: parsed.splToken,
        reference: parsed.reference,
        memo: parsed.memo,
    });

    // 4. Compose and sign the transaction using kit's pipe() pattern
    const { value: latestBlockhash } = await rpc.getLatestBlockhash().send();

    const transactionMessage = pipe(
        createTransactionMessage({ version: 0 }),
        m => setTransactionMessageFeePayer(wallet.address, m),
        m => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
        m => appendTransactionMessageInstructions(instructions, m),
    );

    const compiled = compileTransaction(transactionMessage);
    const signed = await signTransaction([wallet.keyPair], compiled);
    const wireTransaction = getBase64EncodedWireTransaction(signed);

    // 5. Send the transaction
    const signature = await rpc.sendTransaction(wireTransaction, { encoding: 'base64' }).send();
    console.log('Transaction signature:', signature);

    // 6. Merchant finds the transaction by reference
    const found = await findReference(rpc, originalReference);
    console.log('Found signature:', found.signature);
    console.log('Found memo:', found.memo);

    // 7. Merchant validates the transfer
    const response = await validateTransfer(rpc, found.signature, {
        recipient: parsed.recipient,
        amount: parsed.amount!,
        splToken: parsed.splToken,
        reference: parsed.reference,
        memo: parsed.memo,
    });
    console.log('Transfer validated!');
})();
