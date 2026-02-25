import { address, type Address } from '@solana/kit';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { fetchTransaction, FetchTransactionError } from '../src/index.js';

const ACCOUNT = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
const LINK = 'https://example.com/pay';

// Store original fetch
const originalFetch = globalThis.fetch;

function mockFetch(responseBody: any, status = 200) {
    globalThis.fetch = vi.fn().mockResolvedValue({
        ok: status >= 200 && status < 300,
        status,
        json: vi.fn().mockResolvedValue(responseBody),
    });
}

function createMockRpc() {
    return {
        getLatestBlockhash(_opts?: any) {
            return {
                send: vi.fn().mockResolvedValue({
                    value: {
                        blockhash: '4NpjLLnFBqFzwFkRFBauGFYVnijcQhLBSPo9UhbZgqRf' as any,
                        lastValidBlockHeight: 100n,
                    },
                }),
            };
        },
    } as any;
}

describe('FetchTransactionError', () => {
    it('should create error with correct name', () => {
        const err = new FetchTransactionError('test');
        expect(err.name).toBe('FetchTransactionError');
        expect(err).toBeInstanceOf(Error);
    });
});

describe('fetchTransaction', () => {
    afterEach(() => {
        globalThis.fetch = originalFetch;
    });

    it('should POST to the link with correct headers and body', async () => {
        mockFetch({ transaction: 'not-real-base64' });
        const rpc = createMockRpc();

        // Will throw on decode but we can verify the fetch call
        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow(FetchTransactionError);

        expect(globalThis.fetch).toHaveBeenCalledWith(
            LINK,
            expect.objectContaining({
                method: 'POST',
                headers: expect.objectContaining({
                    'Content-Type': 'application/json',
                    Accept: 'application/json',
                }),
                body: JSON.stringify({ account: ACCOUNT }),
            })
        );
    });

    it('should throw FetchTransactionError on non-ok HTTP status', async () => {
        mockFetch({ error: 'not found' }, 404);
        const rpc = createMockRpc();

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow(FetchTransactionError);
    });

    it('should throw "missing transaction" when response has no transaction', async () => {
        mockFetch({});
        const rpc = createMockRpc();

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow('missing transaction');
    });

    it('should throw "missing transaction" when transaction is null', async () => {
        mockFetch({ transaction: null });
        const rpc = createMockRpc();

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow('missing transaction');
    });

    it('should throw "invalid transaction" when transaction is not a string', async () => {
        mockFetch({ transaction: 123 });
        const rpc = createMockRpc();

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow('invalid transaction');
    });

    it('should throw "invalid transaction" when transaction is an object', async () => {
        mockFetch({ transaction: { data: 'abc' } });
        const rpc = createMockRpc();

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow('invalid transaction');
    });

    it('should throw FetchTransactionError on network error', async () => {
        globalThis.fetch = vi.fn().mockRejectedValue(new TypeError('Failed to fetch'));
        const rpc = createMockRpc();

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow(FetchTransactionError);

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow('network error');
    });

    it('should throw FetchTransactionError on invalid JSON response', async () => {
        globalThis.fetch = vi.fn().mockResolvedValue({
            ok: true,
            status: 200,
            json: vi.fn().mockRejectedValue(new SyntaxError('Unexpected token')),
        });
        const rpc = createMockRpc();

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow(FetchTransactionError);

        await expect(fetchTransaction(rpc, ACCOUNT, LINK)).rejects.toThrow('not valid JSON');
    });

    it('should accept URL object as link parameter', async () => {
        mockFetch({});
        const rpc = createMockRpc();

        await expect(fetchTransaction(rpc, ACCOUNT, new URL(LINK))).rejects.toThrow('missing transaction');

        expect(globalThis.fetch).toHaveBeenCalledWith(LINK, expect.anything());
    });
});
