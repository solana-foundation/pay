import type { Address, GetLatestBlockhashApi, Rpc, Transaction } from '@solana/kit';
import {
    AccountRole,
    address,
    appendTransactionMessageInstructions,
    blockhash,
    compileTransaction,
    compressTransactionMessageUsingAddressLookupTables,
    createNoopSigner,
    createTransactionMessage,
    decompileTransactionMessage,
    generateKeyPairSigner,
    getBase64EncodedWireTransaction,
    getCompiledTransactionMessageDecoder,
    partiallySignTransactionMessageWithSigners,
    pipe,
    setTransactionMessageFeePayerSigner,
    setTransactionMessageLifetimeUsingBlockhash,
    signTransactionMessageWithSigners,
} from '@solana/kit';
import { getTransferSolInstruction } from '@solana-program/system';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { fetchTransaction, FetchTransactionError } from '../src/fetchTransaction.js';

const SENDER = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
const RECIPIENT = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const REFERENCE = address('82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny');
const STATIC_ACCOUNT = address('7dHbWXmci3dT1h5tC8S1ZLw6KcDk4chx6Y6bx4dM3f1h');
const TABLE = address('GfC73miMwXBoRYDn7gvEZVbhM7n6SUHxJb4LdBz2Mfp6');
const OTHER_TABLE = address('So11111111111111111111111111111111111111112');
const OLD_BLOCKHASH = blockhash('4NpjLLnFBqFzwFkRFBauGFYVnijcQhLBSPo9UhbZgqRf');
const NEW_BLOCKHASH = blockhash('11111111111111111111111111111111');
const LINK = 'https://merchant.example/pay';
const TABLE_CONTENTS = {
    [TABLE]: [SENDER, RECIPIENT],
    [OTHER_TABLE]: [SENDER, REFERENCE],
};

const originalFetch = globalThis.fetch;
afterEach(() => {
    globalThis.fetch = originalFetch;
});

function createFixture(useLookups = true, source: Address = SENDER) {
    const transfer = getTransferSolInstruction({
        source: createNoopSigner(source),
        destination: RECIPIENT,
        amount: 1_000_000_000n,
    });
    const message = pipe(
        createTransactionMessage({ version: 0 }),
        m => setTransactionMessageFeePayerSigner(createNoopSigner(SENDER), m),
        m => setTransactionMessageLifetimeUsingBlockhash({ blockhash: OLD_BLOCKHASH, lastValidBlockHeight: 1n }, m),
        m =>
            appendTransactionMessageInstructions(
                [
                    {
                        ...transfer,
                        accounts: [...transfer.accounts, { address: REFERENCE, role: AccountRole.READONLY }],
                    },
                ],
                m,
            ),
        m => (useLookups ? compressTransactionMessageUsingAddressLookupTables(m, TABLE_CONTENTS) : m),
    );
    return compileTransaction(message);
}

function mockMerchant(transaction: Transaction) {
    globalThis.fetch = vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        json: async () => ({ transaction: getBase64EncodedWireTransaction(transaction) }),
    });
}

function parsedTable(addresses: readonly Address[]) {
    return {
        data: { parsed: { info: { addresses }, type: 'lookupTable' }, program: 'address-lookup-table', space: 120n },
        executable: false,
        lamports: 1n,
        owner: address('AddressLookupTab1e1111111111111111111111111'),
        rentEpoch: 0n,
        space: 120n,
    };
}

function createRpc() {
    const getLatestBlockhash = vi.fn().mockReturnValue({
        send: vi.fn().mockResolvedValue({
            context: { slot: 1n },
            value: { blockhash: NEW_BLOCKHASH, lastValidBlockHeight: 100n },
        }),
    });
    const sendTables = vi.fn(async () => ({
        context: { slot: 1n },
        value: [parsedTable(TABLE_CONTENTS[TABLE]), parsedTable(TABLE_CONTENTS[OTHER_TABLE])],
    }));
    const getMultipleAccounts = vi.fn().mockReturnValue({ send: sendTables });
    const rpc = { getLatestBlockhash, getMultipleAccounts } as unknown as Parameters<typeof fetchTransaction>[0];
    return { rpc, getLatestBlockhash, getMultipleAccounts, sendTables };
}

describe('fetchTransaction address lookup tables', () => {
    it('resolves writable and readonly lookups at nonzero indexes and preserves the instructions', async () => {
        const original = createFixture();
        mockMerchant(original);
        const { rpc, getLatestBlockhash, getMultipleAccounts } = createRpc();
        const result = await fetchTransaction(rpc, SENDER, LINK, { commitment: 'finalized' });
        const before = getCompiledTransactionMessageDecoder().decode(original.messageBytes);
        const after = getCompiledTransactionMessageDecoder().decode(result.messageBytes);
        if (before.version !== 0 || after.version !== 0) throw new Error('expected v0');
        if (!before.addressTableLookups || !after.addressTableLookups) throw new Error('expected address lookups');

        expect(before.addressTableLookups).toHaveLength(2);
        expect(after.addressTableLookups).toEqual(before.addressTableLookups);
        expect(after.addressTableLookups.flatMap(t => [...t.writableIndexes, ...t.readonlyIndexes])).toEqual([1, 1]);
        const decoded = decompileTransactionMessage(after, { addressesByLookupTableAddress: TABLE_CONTENTS });
        const originalDecoded = decompileTransactionMessage(before, { addressesByLookupTableAddress: TABLE_CONTENTS });
        expect(decoded.instructions).toEqual(originalDecoded.instructions);
        expect(after.staticAccounts[0]).toBe(SENDER);
        expect(after.lifetimeToken).toBe(NEW_BLOCKHASH);
        expect(Object.keys(result.signatures)).toEqual([SENDER]);
        expect(getLatestBlockhash).toHaveBeenCalledWith({ commitment: 'finalized' });
        expect(getMultipleAccounts).toHaveBeenCalledWith([TABLE, OTHER_TABLE], {
            encoding: 'jsonParsed',
            commitment: 'finalized',
        });
    });

    it('updates the fee payer when it is already a static instruction signer', async () => {
        mockMerchant(createFixture(true, STATIC_ACCOUNT));
        const { rpc } = createRpc();
        const result = await fetchTransaction(rpc, STATIC_ACCOUNT, LINK);
        const compiled = getCompiledTransactionMessageDecoder().decode(result.messageBytes);
        expect(compiled.staticAccounts[0]).toBe(STATIC_ACCOUNT);
        const decoded = decompileTransactionMessage(compiled, { addressesByLookupTableAddress: TABLE_CONTENTS });
        expect(decoded.instructions[0].accounts?.map(a => a.address)).toEqual([STATIC_ACCOUNT, RECIPIENT, REFERENCE]);
        expect(compiled.staticAccounts).not.toContain(SENDER);
        expect(Object.keys(result.signatures)).toEqual([STATIC_ACCOUNT]);
        expect(decoded.instructions[0].data).toEqual(
            getTransferSolInstruction({
                source: createNoopSigner(STATIC_ACCOUNT),
                destination: RECIPIENT,
                amount: 1_000_000_000n,
            }).data,
        );
    });

    it('moves a fee payer out of a lookup without losing the other loaded accounts', async () => {
        mockMerchant(createFixture());
        const { rpc } = createRpc();
        const result = await fetchTransaction(rpc, RECIPIENT, LINK);
        const compiled = getCompiledTransactionMessageDecoder().decode(result.messageBytes);
        if (compiled.version !== 0) throw new Error('expected v0');
        expect(compiled.staticAccounts[0]).toBe(RECIPIENT);
        expect(compiled.addressTableLookups).toEqual([
            { lookupTableAddress: OTHER_TABLE, writableIndexes: [], readonlyIndexes: [1] },
        ]);
        const decoded = decompileTransactionMessage(compiled, { addressesByLookupTableAddress: TABLE_CONTENTS });
        expect(decoded.instructions[0].accounts?.map(a => a.address)).toEqual([SENDER, RECIPIENT, REFERENCE]);
    });

    it('keeps blockhash-only RPC clients working for transactions without lookup tables', async () => {
        mockMerchant(createFixture(false));
        const { getLatestBlockhash } = createRpc();
        const rpc = { getLatestBlockhash } as unknown as Rpc<GetLatestBlockhashApi>;
        await expect(fetchTransaction(rpc, SENDER, LINK)).resolves.toBeDefined();
    });

    it('does not look up accounts when a v0 transaction has no lookup tables', async () => {
        mockMerchant(createFixture(false));
        const { rpc, getMultipleAccounts } = createRpc();
        await fetchTransaction(rpc, SENDER, LINK);
        expect(getMultipleAccounts).not.toHaveBeenCalled();
    });

    it('reports a missing lookup RPC method as FetchTransactionError', async () => {
        mockMerchant(createFixture());
        const { getLatestBlockhash } = createRpc();
        const rpc = { getLatestBlockhash } as unknown as Rpc<GetLatestBlockhashApi>;
        await expect(fetchTransaction(rpc, SENDER, LINK)).rejects.toThrow(FetchTransactionError);
        await expect(fetchTransaction(rpc, SENDER, LINK)).rejects.toThrow('getMultipleAccounts');
    });

    it('reports lookup RPC failures as FetchTransactionError', async () => {
        mockMerchant(createFixture());
        const { rpc, sendTables } = createRpc();
        sendTables.mockRejectedValue(new Error('fixture RPC unavailable'));
        await expect(fetchTransaction(rpc, SENDER, LINK)).rejects.toThrow(FetchTransactionError);
        await expect(fetchTransaction(rpc, SENDER, LINK)).rejects.toThrow('fixture RPC unavailable');
    });

    it.each(['missing', 'truncated', 'malformed'] as const)(
        'rejects %s table data with FetchTransactionError',
        async kind => {
            mockMerchant(createFixture());
            const { rpc, sendTables } = createRpc();
            const first =
                kind === 'missing'
                    ? null
                    : kind === 'truncated'
                      ? parsedTable([SENDER])
                      : {
                            ...parsedTable([]),
                            data: { parsed: { info: {}, type: 'lookupTable' }, program: 'address-lookup-table' },
                        };
            sendTables.mockResolvedValue({
                context: { slot: 1n },
                value: [first, parsedTable(TABLE_CONTENTS[OTHER_TABLE])],
            } as never);
            await expect(fetchTransaction(rpc, SENDER, LINK)).rejects.toThrow(FetchTransactionError);
        },
    );

    it.each([null, 'bad'])(
        'rejects invalid lookup address elements (%s) before rebuilding the transaction',
        async invalidAddress => {
            mockMerchant(createFixture());
            const { rpc, sendTables } = createRpc();
            // Repeated malformed addresses must not merge the writable recipient and readonly reference.
            const malformedTable = {
                ...parsedTable([]),
                data: {
                    parsed: { info: { addresses: [invalidAddress, invalidAddress] }, type: 'lookupTable' },
                    program: 'address-lookup-table',
                },
            };
            sendTables.mockResolvedValue({
                context: { slot: 1n },
                value: [malformedTable, malformedTable],
            } as never);
            await expect(fetchTransaction(rpc, SENDER, LINK)).rejects.toThrow(FetchTransactionError);
            await expect(fetchTransaction(rpc, SENDER, LINK)).rejects.toThrow('failed to prepare unsigned transaction');
        },
    );

    it.each([false, true])(
        'preserves signed lookup transactions without RPC calls (wallet signature pending: %s)',
        async pending => {
            const signer = await generateKeyPairSigner();
            const wallet = createNoopSigner(SENDER);
            const message = pipe(
                createTransactionMessage({ version: 0 }),
                m => setTransactionMessageFeePayerSigner(signer, m),
                m =>
                    setTransactionMessageLifetimeUsingBlockhash(
                        { blockhash: OLD_BLOCKHASH, lastValidBlockHeight: 1n },
                        m,
                    ),
                m =>
                    appendTransactionMessageInstructions(
                        [
                            getTransferSolInstruction({
                                source: pending ? wallet : signer,
                                destination: RECIPIENT,
                                amount: 1n,
                            }),
                        ],
                        m,
                    ),
                m => compressTransactionMessageUsingAddressLookupTables(m, TABLE_CONTENTS),
            );
            const original = pending
                ? await partiallySignTransactionMessageWithSigners(message)
                : await signTransactionMessageWithSigners(message);
            mockMerchant(original);
            const { rpc, getLatestBlockhash, getMultipleAccounts } = createRpc();
            const result = await fetchTransaction(rpc, pending ? wallet.address : signer.address, LINK);
            expect(result.messageBytes).toEqual(original.messageBytes);
            expect(result.signatures).toEqual(original.signatures);
            expect(getLatestBlockhash).not.toHaveBeenCalled();
            expect(getMultipleAccounts).not.toHaveBeenCalled();
        },
    );
});
