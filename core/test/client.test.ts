import { address, type Address, type TransactionSigner } from '@solana/kit';
import { describe, it, expect, vi } from 'vitest';
import { createSolanaPayClient } from '../src/client.js';

function createMockSigner(addr: Address): TransactionSigner {
    return {
        address: addr,
        signTransactions: vi.fn(),
    } as unknown as TransactionSigner;
}

vi.mock('@solana/kit-plugin-rpc', () => ({
    rpc: (url: string) => (client: any) => ({ ...client, rpc: { __mockRpcUrl: url } }),
}));

vi.mock('@solana/kit-plugin-payer', () => ({
    payer: (signer: TransactionSigner) => (client: any) => ({ ...client, payer: signer }),
}));

describe('createSolanaPayClient', () => {
    const rpcUrl = 'https://api.mainnet-beta.solana.com';
    const mockPayer = createMockSigner(address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa'));

    it('should return a client with pay namespace', () => {
        const client = createSolanaPayClient({ rpcUrl, payer: mockPayer });

        expect(client.pay).toBeDefined();
    });

    it('should expose all solanaPay methods', () => {
        const client = createSolanaPayClient({ rpcUrl, payer: mockPayer });

        expect(typeof client.pay.createTransfer).toBe('function');
        expect(typeof client.pay.encodeURL).toBe('function');
        expect(typeof client.pay.parseURL).toBe('function');
        expect(typeof client.pay.createQR).toBe('function');
        expect(typeof client.pay.createQROptions).toBe('function');
        expect(typeof client.pay.findReference).toBe('function');
        expect(typeof client.pay.validateTransfer).toBe('function');
        expect(typeof client.pay.fetchTransaction).toBe('function');
    });

    it('should configure rpc from rpcUrl', () => {
        const client = createSolanaPayClient({ rpcUrl, payer: mockPayer });

        expect((client.rpc as any).__mockRpcUrl).toBe(rpcUrl);
    });

    it('should configure payer from config', () => {
        const client = createSolanaPayClient({ rpcUrl, payer: mockPayer });

        expect(client.payer).toBe(mockPayer);
    });

    it('should return a frozen object', () => {
        const client = createSolanaPayClient({ rpcUrl, payer: mockPayer });

        expect(Object.isFrozen(client)).toBe(true);
    });

    it('should encode and parse URLs via pay namespace', () => {
        const client = createSolanaPayClient({ rpcUrl, payer: mockPayer });
        const recipient = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');

        const url = client.pay.encodeURL({ recipient, amount: 1.5 });
        const parsed = client.pay.parseURL(url);

        expect(parsed.recipient).toBe(recipient);
        expect('amount' in parsed && parsed.amount).toBe(1.5);
    });
});
