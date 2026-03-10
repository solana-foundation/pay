import { type Address, address, type TransactionSigner } from '@solana/kit';
import { describe, expect, it, vi } from 'vitest';

import { createMerchantClient, createSolanaPayClient, createWalletClient } from '../src/client.js';

function createMockSigner(addr: Address): TransactionSigner {
    return {
        address: addr,
        signTransactions: vi.fn(),
    } as unknown as TransactionSigner;
}

vi.mock('@solana/kit-plugin-rpc', () => ({
    rpc: (url: string) => (client: any) => ({ ...client, rpc: { __mockRpcUrl: url } }),
    rpcTransactionPlanner: () => (client: any) => ({ ...client, transactionPlanner: vi.fn() }),
    rpcTransactionPlanExecutor: () => (client: any) => ({ ...client, transactionPlanExecutor: vi.fn() }),
}));

vi.mock('@solana/kit-plugin-payer', () => ({
    payer: (signer: TransactionSigner) => (client: any) => ({ ...client, payer: signer }),
}));

vi.mock('@solana/kit-plugin-instruction-plan', () => ({
    planAndSendTransactions: () => (client: any) => ({
        ...client,
        sendTransaction: vi.fn(),
        sendTransactions: vi.fn(),
    }),
}));

const rpcUrl = 'https://api.mainnet-beta.solana.com';
const mockPayer = createMockSigner(address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa'));

describe('createMerchantClient', () => {
    it('should return a client with merchant pay methods', () => {
        const client = createMerchantClient({ rpcUrl });

        expect(typeof client.pay.encodeURL).toBe('function');
        expect(typeof client.pay.createQR).toBe('function');
        expect(typeof client.pay.createQROptions).toBe('function');
        expect(typeof client.pay.findReference).toBe('function');
        expect(typeof client.pay.validateTransfer).toBe('function');
    });

    it('should not include wallet methods', () => {
        const client = createMerchantClient({ rpcUrl }) as any;

        expect(client.pay.parseURL).toBeUndefined();
        expect(client.pay.createTransfer).toBeUndefined();
        expect(client.pay.fetchTransaction).toBeUndefined();
    });

    it('should configure rpc from rpcUrl', () => {
        const client = createMerchantClient({ rpcUrl });

        expect((client.rpc as any).__mockRpcUrl).toBe(rpcUrl);
    });

    it('should not have a payer', () => {
        const client = createMerchantClient({ rpcUrl }) as any;

        expect(client.payer).toBeUndefined();
    });

    it('should return a frozen object', () => {
        const client = createMerchantClient({ rpcUrl });

        expect(Object.isFrozen(client)).toBe(true);
    });

    it('should encode URLs via pay namespace', () => {
        const client = createMerchantClient({ rpcUrl });
        const recipient = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');

        const url = client.pay.encodeURL({ recipient, amount: 1.5 });

        expect(url).toBeInstanceOf(URL);
        expect(url.protocol).toBe('solana:');
    });
});

describe('createWalletClient', () => {
    it('should return a client with wallet pay methods', () => {
        const client = createWalletClient({ rpcUrl, payer: mockPayer });

        expect(typeof client.pay.parseURL).toBe('function');
        expect(typeof client.pay.createTransfer).toBe('function');
        expect(typeof client.pay.fetchTransaction).toBe('function');
    });

    it('should not include merchant methods', () => {
        const client = createWalletClient({ rpcUrl, payer: mockPayer }) as any;

        expect(client.pay.encodeURL).toBeUndefined();
        expect(client.pay.createQR).toBeUndefined();
        expect(client.pay.findReference).toBeUndefined();
        expect(client.pay.validateTransfer).toBeUndefined();
    });

    it('should configure rpc and payer', () => {
        const client = createWalletClient({ rpcUrl, payer: mockPayer });

        expect((client.rpc as any).__mockRpcUrl).toBe(rpcUrl);
        expect(client.payer).toBe(mockPayer);
    });

    it('should return a frozen object', () => {
        const client = createWalletClient({ rpcUrl, payer: mockPayer });

        expect(Object.isFrozen(client)).toBe(true);
    });

    it('should parse URLs via pay namespace', () => {
        const client = createWalletClient({ rpcUrl, payer: mockPayer });
        const recipient = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');

        const parsed = client.pay.parseURL(`solana:${recipient}?amount=1`);

        expect(parsed.recipient).toBe(recipient);
    });
});

describe('createSolanaPayClient', () => {
    it('should return a client with all pay methods', () => {
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

    it('should configure rpc and payer', () => {
        const client = createSolanaPayClient({ rpcUrl, payer: mockPayer });

        expect((client.rpc as any).__mockRpcUrl).toBe(rpcUrl);
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
