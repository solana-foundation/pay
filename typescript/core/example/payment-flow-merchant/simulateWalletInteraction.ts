import type { Connection } from '@solana/web3.js';
import { LAMPORTS_PER_SOL, sendAndConfirmTransaction } from '@solana/web3.js';
import BigNumber from 'bignumber.js';
import type { TransferRequestURL } from '../../lib/types.js';
import { createTransfer, parseURL } from '../../src.js';
import { CUSTOMER_WALLET } from './constants.js';

export async function simulateWalletInteraction(connection: Connection, url: URL) {
    /**
     * For example only
     *
     * The URL that triggers the wallet interaction; follows the Solana Pay URL scheme
     * The parameters needed to create the correct transaction is encoded within the URL
     */
    const { recipient, amount, reference, label, message, memo } = parseURL(url) as TransferRequestURL;
    console.log('label: ', label);
    console.log('message: ', message);

    /**
     * For example only
     *
     * Attempts to airdrop the customers wallet some SOL for a succeful transaction
     */
    await getPayer(connection);

    /**
     * Create the transaction with the parameters decoded from the URL
     */
    const tx = await createTransfer(connection, CUSTOMER_WALLET.publicKey, { recipient, amount, reference, memo });

    /**
     * Send the transaction to the network
     */
    sendAndConfirmTransaction(connection, tx, [CUSTOMER_WALLET]);
}

async function getPayer(connection: Connection) {
    try {
        const airdropSignature = await connection.requestAirdrop(CUSTOMER_WALLET.publicKey, LAMPORTS_PER_SOL * 2);
        await connection.confirmTransaction(airdropSignature);
    } catch (error) {
        // Fail silently
    }
    await new Promise((resolve) => setTimeout(resolve, 5000));

    return;
}
