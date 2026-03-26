import { Keypair, PublicKey } from '@solana/web3.js';

export const MERCHANT_WALLET = new PublicKey('mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN');

// Keypair purely for testing purposes. Exists only on devnet
export const CUSTOMER_WALLET = Keypair.fromSecretKey(
    Uint8Array.from([
        169, 48, 146, 127, 191, 185, 98, 158, 130, 159, 205, 137, 2, 146, 85, 1, 93, 107, 98, 90, 245, 69, 40, 39, 220,
        78, 226, 249, 231, 254, 92, 13, 186, 138, 174, 147, 156, 143, 248, 132, 28, 206, 134, 228, 241, 192, 94, 44,
        177, 15, 41, 219, 124, 116, 255, 78, 172, 209, 106, 78, 37, 169, 115, 146,
    ])
);
