import type { PublicKey } from '@solana/web3.js';
import { Keypair } from '@solana/web3.js';
import BigNumber from 'bignumber.js';

/**
 * Simulate a checkout experience
 *
 * Recommendation:
 * `amount` and `reference` should be created in a trusted environment (server).
 * The `reference` should be unique to a single customer session,
 * and will be used to find and validate the payment in the future.
 *
 * Read our [getting started guide](#getting-started-guide) for more information on what these parameters mean.
 */
export async function simulateCheckout(): Promise<{
    label: string;
    message: string;
    memo: string;
    amount: BigNumber;
    reference: PublicKey;
}> {
    return {
        label: 'Jungle Cats store',
        message: 'Jungle Cats store - your order - #001234',
        memo: 'JC#4098',
        amount: new BigNumber(1),
        reference: new Keypair().publicKey,
    };
}
