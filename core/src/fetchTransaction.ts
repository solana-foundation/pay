import type {
    Address,
    Commitment,
    Rpc,
    GetLatestBlockhashApi,
    Transaction,
    ReadonlyUint8Array,
    CompiledTransactionMessage,
    CompiledTransactionMessageWithLifetime,
} from '@solana/kit';
import {
    getBase64Encoder,
    getAddressEncoder,
    getTransactionDecoder,
    getCompiledTransactionMessageDecoder,
    decompileTransactionMessage,
    setTransactionMessageFeePayer,
    setTransactionMessageLifetimeUsingBlockhash,
    compileTransaction,
    address,
} from '@solana/kit';
import { verifySignature, assertIsSignatureBytes } from '@solana/keys';

/**
 * Thrown when a transaction response can't be fetched.
 */
export class FetchTransactionError extends Error {
    name = 'FetchTransactionError';
}

/**
 * A compiled transaction with message bytes and signatures.
 */
export type FetchedTransaction = Transaction;

/**
 * Fetch a transaction from a Solana Pay transaction request link.
 *
 * @param rpc - An RPC client supporting `getLatestBlockhash`.
 * @param account - Address of the account that may sign the transaction.
 * @param link - `link` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#link).
 * @param options - Options for `getLatestBlockhash`.
 *
 * @throws {FetchTransactionError}
 */
export async function fetchTransaction(
    rpc: Rpc<GetLatestBlockhashApi>,
    account: Address,
    link: string | URL,
    { commitment }: { commitment?: Commitment } = {}
): Promise<FetchedTransaction> {
    let response: Response;
    try {
        response = await fetch(String(link), {
            method: 'POST',
            mode: 'cors',
            credentials: 'omit',
            headers: {
                'Cache-Control': 'no-cache',
                Accept: 'application/json',
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({ account }),
        });
    } catch (error) {
        throw new FetchTransactionError(`network error: ${error instanceof Error ? error.message : String(error)}`);
    }

    if (!response.ok) throw new FetchTransactionError(`request failed: ${response.status}`);

    let json: Record<string, unknown>;
    try {
        json = (await response.json()) as Record<string, unknown>;
    } catch {
        throw new FetchTransactionError('response is not valid JSON');
    }
    if (!json?.transaction) throw new FetchTransactionError('missing transaction');
    if (typeof json.transaction !== 'string') throw new FetchTransactionError('invalid transaction');

    // Decode the base64 transaction string to bytes, then decode the transaction
    let transactionBytes: ReadonlyUint8Array;
    try {
        transactionBytes = getBase64Encoder().encode(json.transaction);
    } catch {
        throw new FetchTransactionError('invalid base64 in transaction');
    }

    let transaction: Transaction;
    try {
        transaction = getTransactionDecoder().decode(transactionBytes);
    } catch {
        throw new FetchTransactionError('failed to decode transaction wire format');
    }

    let compiledMessage: CompiledTransactionMessage & CompiledTransactionMessageWithLifetime;
    try {
        compiledMessage = getCompiledTransactionMessageDecoder().decode(transaction.messageBytes);
    } catch {
        throw new FetchTransactionError('failed to decode compiled transaction message');
    }

    // Extract signatures map
    const signatures = transaction.signatures;
    const signerAddresses = Object.keys(signatures).map((addr) => address(addr));

    const hasSignatures = signerAddresses.some((addr) => {
        const sig = signatures[addr];
        return sig != null && !sig.every((b: number) => b === 0);
    });

    if (hasSignatures) {
        const feePayer = signerAddresses[0];
        if (!feePayer) throw new FetchTransactionError('missing fee payer');

        if (compiledMessage.staticAccounts.length === 0 || compiledMessage.staticAccounts[0] !== feePayer) {
            throw new FetchTransactionError('invalid fee payer');
        }

        if (!compiledMessage.lifetimeToken) {
            throw new FetchTransactionError('missing recent blockhash');
        }

        // A valid signature for everything except `account` must be provided.
        const addressEncoder = getAddressEncoder();
        for (const addr of signerAddresses) {
            const sig = signatures[addr];
            const isNonZero = sig != null && !sig.every((b: number) => b === 0);

            if (isNonZero) {
                const publicKeyBytes = addressEncoder.encode(addr);
                const cryptoKey = await crypto.subtle.importKey('raw', publicKeyBytes, { name: 'Ed25519' }, false, [
                    'verify',
                ]);
                assertIsSignatureBytes(sig);
                const isValid = await verifySignature(cryptoKey, sig, transaction.messageBytes);
                if (!isValid) throw new FetchTransactionError('invalid signature');
            } else if (addr === account) {
                // If the only signature needed is for `account`, refresh the blockhash
                if (signerAddresses.length === 1) {
                    const { value } = await rpc.getLatestBlockhash({ commitment }).send();
                    const msg = decompileTransactionMessage(compiledMessage);
                    const updatedMsg = setTransactionMessageLifetimeUsingBlockhash(value, msg);
                    return compileTransaction(updatedMsg);
                }
            } else {
                throw new FetchTransactionError('missing signature');
            }
        }

        return transaction;
    } else {
        // Ignore the fee payer and recent blockhash in the transaction and initialize them.
        const { value } = await rpc.getLatestBlockhash({ commitment }).send();
        const msg = decompileTransactionMessage(compiledMessage);
        const withFeePayer = setTransactionMessageFeePayer(account, msg);
        const withLifetime = setTransactionMessageLifetimeUsingBlockhash(value, withFeePayer);
        return compileTransaction(withLifetime);
    }
}
