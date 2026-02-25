import type { Address } from '@solana/kit';
import { address } from '@solana/kit';

export const MAX_CONFIRMATIONS = 32;

export const NON_BREAKING_SPACE = '\u00a0';

export const DEVNET_ENDPOINT = 'https://api.devnet.solana.com';

export const MAINNET_ENDPOINT = 'https://solana-mainnet.phantom.tech';

// Mint DUMMY tokens on devnet @ https://spl-token-faucet.com
export const DEVNET_DUMMY_MINT: Address = address('Gh9ZwEmdLJ8DscKNTkTqPbNwLNNBjuSzaG9Vp2KGtKJr');

export const MAINNET_USDC_MINT: Address = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
