import { address } from 'gill';
import type { SPLToken, Recipient } from './types';

/**
 * Safely convert a string address to SPLToken type
 */
export function createSPLToken(tokenAddress: string): SPLToken {
    try {
        return address(tokenAddress);
    } catch (error) {
        throw new Error(`Invalid SPL token address: ${tokenAddress}. ${error}`);
    }
}

/**
 * Safely convert a string address to Recipient type  
 */
export function createRecipient(recipientAddress: string): Recipient {
    try {
        return address(recipientAddress);
    } catch (error) {
        throw new Error(`Invalid recipient address: ${recipientAddress}. ${error}`);
    }
}

/**
 * Validate if a string is a valid Solana address
 */
export function isValidSolanaAddress(addr: string): boolean {
    try {
        address(addr);
        return true;
    } catch {
        return false;
    }
}