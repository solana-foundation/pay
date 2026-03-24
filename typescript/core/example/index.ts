import { createAssociatedTokenAccount } from '@solana/spl-token';
import { clusterApiUrl, Connection, Keypair, LAMPORTS_PER_SOL, sendAndConfirmRawTransaction } from '@solana/web3.js';
import type { TransferRequestURL } from '../src.js';
import { createTransfer, encodeURL, findReference, parseURL, validateTransfer } from '../src.js';

(async function () {
    const cluster = 'devnet';
    const endpoint = clusterApiUrl(cluster);
    const connection = new Connection(endpoint, 'confirmed');

    // Merchant app generates a random public key referenced by the transaction, in order to locate it after it's sent
    const originalReference = Keypair.generate().publicKey;

    const NATIVE_URL =
        'solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN' +
        '?amount=0.01' +
        '&reference=' +
        encodeURIComponent(String(originalReference)) +
        '&label=Michael' +
        '&message=Thanks%20for%20all%20the%20fish' +
        '&memo=OrderId5678';

    const USDC_URL =
        'solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN' +
        '?amount=0.01' +
        '&spl-token=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v' +
        '&reference=' +
        encodeURIComponent(String(originalReference)) +
        '&label=Michael' +
        '&message=Thanks%20for%20all%20the%20fish' +
        '&memo=OrderId5678';

    const originalURL = NATIVE_URL;
    console.log(originalURL);

    // Wallet gets URL from deep link / QR code
    const { recipient, amount, splToken, reference, label, message, memo } = parseURL(
        originalURL
    ) as TransferRequestURL;

    // Apps can encode the URL from the required and optional parameters
    const encodedURL = encodeURL({ recipient, amount, splToken, reference, label, message, memo });

    console.log(originalURL);
    console.log(encodedURL);

    // This just represents the wallet's keypair for testing, in practice it will have a way of signing already
    const wallet = Keypair.generate();

    const airdrop = await connection.requestAirdrop(wallet.publicKey, LAMPORTS_PER_SOL);
    await connection.confirmTransaction(airdrop, 'confirmed');

    if (splToken) {
        await createAssociatedTokenAccount(connection, wallet, splToken, wallet.publicKey, { commitment: 'confirmed' });
    }

    // Create a transaction to transfer native SOL or SPL tokens
    const transaction = await createTransfer(connection, wallet.publicKey, {
        recipient,
        amount,
        splToken,
        reference,
        memo,
    });

    // Sign, send, and confirm the transaction
    transaction.sign(wallet);

    const rawTransaction = transaction.serialize();
    const signature = await sendAndConfirmRawTransaction(connection, rawTransaction);

    console.log(signature);

    // Merchant app locates the transaction signature from the unique reference address it provided in the transfer link
    const found = await findReference(connection, originalReference);

    // Matches the signature of the transaction
    console.log(found.signature);

    // Contains the memo provided, prefixed with its length: `[11] OrderId5678`
    console.log(found.memo);

    // Merchant app should always validate that the transaction transferred the expected amount to the recipient
    const response = await validateTransfer(connection, found.signature, {
        recipient,
        amount,
        splToken,
        reference,
        memo,
    });
})();
