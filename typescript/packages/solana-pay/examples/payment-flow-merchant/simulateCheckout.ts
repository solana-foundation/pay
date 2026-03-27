import { generateKeyPairSigner, type Address } from '@solana/kit';

/**
 * Simulate a checkout experience
 *
 * Recommendation:
 * `amount` and `reference` should be created in a trusted environment (server).
 * The `reference` should be unique to a single customer session,
 * and will be used to find and validate the payment in the future.
 */
export async function simulateCheckout(): Promise<{
    label: string;
    message: string;
    memo: string;
    amount: number;
    reference: Address;
}> {
    const referenceSigner = await generateKeyPairSigner();
    return {
        label: 'Jungle Cats store',
        message: 'Jungle Cats store - your order - #001234',
        memo: 'JC#4098',
        amount: 1,
        reference: referenceSigner.address,
    };
}
