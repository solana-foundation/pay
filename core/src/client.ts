import { createEmptyClient, type TransactionSigner } from '@solana/kit';
import { rpc } from '@solana/kit-plugin-rpc';
import { payer } from '@solana/kit-plugin-payer';
import { solanaPay } from './plugin.js';

/** Configuration for {@link createSolanaPayClient}. */
export interface SolanaPayClientConfig {
    /** Solana RPC URL (e.g. 'https://api.mainnet-beta.solana.com'). */
    rpcUrl: string;
    /** Wallet signer that will be used as the default sender / fee payer. */
    payer: TransactionSigner;
}

/** The type returned by {@link createSolanaPayClient}. */
export type SolanaPayClient = ReturnType<typeof createSolanaPayClient>;

/**
 * Creates a Solana Pay Kit client pre-composed with RPC, payer, and the
 * `solanaPay()` namespace plugin.
 *
 * This is a thin convenience over manually composing the plugins:
 * ```ts
 * createEmptyClient()
 *   .use(rpc(rpcUrl))
 *   .use(payer(signer))
 *   .use(solanaPay())
 * ```
 *
 * @example
 * ```ts
 * import { createSolanaPayClient } from '@solana/pay';
 * import { address } from '@solana/kit';
 *
 * const client = createSolanaPayClient({
 *   rpcUrl: 'https://api.mainnet-beta.solana.com',
 *   payer: myWalletSigner,
 * });
 *
 * // Pure functions
 * const url = client.pay.encodeURL({
 *   recipient: address('...'),
 *   amount: 1.5,
 *   splToken: address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'),
 * });
 *
 * // RPC-bound functions (uses configured rpc + payer)
 * const instructions = await client.pay.createTransfer({
 *   recipient: address('...'),
 *   amount: 1.5,
 * });
 *
 * const { signature } = await client.pay.findReference(referenceAddress);
 * ```
 */
export function createSolanaPayClient(config: SolanaPayClientConfig) {
    return createEmptyClient().use(rpc(config.rpcUrl)).use(payer(config.payer)).use(solanaPay());
}
