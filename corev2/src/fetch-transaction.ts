import {
    Address,
    Rpc,
    SolanaRpcApi,
    Signature,
    Transaction,
} from 'gill';

export async function fetchTransaction(
    rpc: Rpc<SolanaRpcApi>,
    signature: Signature,
): Promise<Transaction> {
    const response = await rpc.getTransaction(signature).send();
    const transaction = response?.transaction;
    if (!transaction) {
        throw new Error('Transaction not found');
    }

    return transaction as unknown as Transaction;
}