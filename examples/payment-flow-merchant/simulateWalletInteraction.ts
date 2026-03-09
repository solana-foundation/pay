import {
    pipe,
    createTransactionMessage,
    setTransactionMessageFeePayer,
    setTransactionMessageLifetimeUsingBlockhash,
    appendTransactionMessageInstructions,
    signTransaction,
    compileTransaction,
    getBase64EncodedWireTransaction,
    lamports,
    type Rpc,
    type GetAccountInfoApi,
    type GetLatestBlockhashApi,
    type SendTransactionApi,
} from '@solana/kit';
import type { TransferRequestURL } from '@solana/pay';
import { createTransfer, parseURL } from '@solana/pay';
import { CUSTOMER_WALLET } from './constants.js';

export async function simulateWalletInteraction(
    rpc: Rpc<GetAccountInfoApi & GetLatestBlockhashApi & SendTransactionApi>,
    url: URL
) {
    /**
     * The URL that triggers the wallet interaction; follows the Solana Pay URL scheme.
     * The parameters needed to create the correct transaction are encoded within the URL.
     */
    const { recipient, amount, reference, label, message, memo } = parseURL(url) as TransferRequestURL;
    console.log('label: ', label);
    console.log('message: ', message);

    /**
     * Airdrop some SOL to the customer wallet for a successful transaction
     */
    try {
        await (rpc as any).requestAirdrop(CUSTOMER_WALLET.address, lamports(2_000_000_000n)).send();
    } catch {
        // Fail silently — airdrop may not be available
    }
    await new Promise((resolve) => setTimeout(resolve, 5000));

    /**
     * Create the transfer instructions from the parsed URL parameters
     */
    const instructions = await createTransfer(rpc, CUSTOMER_WALLET, {
        recipient,
        amount: amount!,
        reference,
        memo,
    });

    /**
     * Build, sign, and send the transaction
     */
    const { value: latestBlockhash } = await rpc.getLatestBlockhash().send();

    const transactionMessage = pipe(
        createTransactionMessage({ version: 0 }),
        (m) => setTransactionMessageFeePayer(CUSTOMER_WALLET.address, m),
        (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
        (m) => appendTransactionMessageInstructions(instructions, m)
    );

    const compiled = compileTransaction(transactionMessage);
    const signed = await signTransaction([CUSTOMER_WALLET.keyPair], compiled);
    const wireTransaction = getBase64EncodedWireTransaction(signed);

    await rpc.sendTransaction(wireTransaction, { encoding: 'base64' }).send();
}
