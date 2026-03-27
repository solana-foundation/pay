import { type Address, address, type Signature } from '@solana/kit';
import { SYSTEM_PROGRAM_ADDRESS } from '@solana-program/system';
import { describe, expect, it, vi } from 'vitest';

import { validateTransfer, ValidateTransferError } from '../src/index.js';

// Mock only findAssociatedTokenPda, keep real parsers/identifiers
vi.mock('@solana-program/token', async importOriginal => {
    const actual = await importOriginal<typeof import('@solana-program/token')>();
    return {
        ...actual,
        findAssociatedTokenPda: vi
            .fn()
            .mockResolvedValue(['GfC73miMwXBoRYDn7gvEZVbhM7n6SUHxJb4LdBz2Mfp6' as Address, 255]),
    };
});

const SIGNATURE = '5UfDuX7hXbDBZpHnSEFMwBN6JdANTF54fGVz9Kp1fZBNTmRmEiGP' as Signature;

const ADDRESSES = {
    sender: address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa'),
    recipient: address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'),
    reference: address('82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny'),
    senderATA: address('7dHbWXmci3dT1h5tC8S1ZLw6KcDk4chx6Y6bx4dM3f1h'),
    splToken: address('So11111111111111111111111111111111111111112'),
    recipientATA: address('GfC73miMwXBoRYDn7gvEZVbhM7n6SUHxJb4LdBz2Mfp6'),
};

/**
 * Pre-compiled base64 wire-format transactions.
 * Generated from real @solana/kit compiled transaction messages.
 *
 * Static account order for SOL transfers: [sender(0), recipient(1), systemProgram(2)]
 * Static account order for SPL transfers: [sender(0), senderATA(1), recipientATA(2), splToken(3), tokenProgram(4)]
 */
const TX = {
    /** SOL transfer: sender → recipient, 1 SOL. Accounts: [sender, recipient, system] */
    SOL_TRANSFER:
        'AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAED253ynD68LT6pZYKXCxW2ZdXbKibEiRnYD8UPXLZkNuPG+nrzvtutOj1l82qryXQxsbvkwtL24OR8pgIDRS9dYQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAgIAAQwCAAAAAMqaOwAAAAA=',
    /** SOL transfer + reference key. Accounts: [sender, recipient, system, reference] */
    SOL_TRANSFER_WITH_REF:
        'AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAIE253ynD68LT6pZYKXCxW2ZdXbKibEiRnYD8UPXLZkNuPG+nrzvtutOj1l82qryXQxsbvkwtL24OR8pgIDRS9dYQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAaGfnzwpgxJvAvNES3A/u4ApvkfeqH7Qky9uxLnjnlDIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAECAwABAwwCAAAAAMqaOwAAAAA=',
    /** Memo("test") + SOL transfer. Accounts: [sender, recipient, system, memoProg] */
    SOL_TRANSFER_WITH_MEMO:
        'AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAIE253ynD68LT6pZYKXCxW2ZdXbKibEiRnYD8UPXLZkNuPG+nrzvtutOj1l82qryXQxsbvkwtL24OR8pgIDRS9dYQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABUpTWpkpIQZNJOhxYNo4fHw1td28kruB5B+oQEEFRI0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIDAQAEdGVzdAICAAEMAgAAAADKmjsAAAAA',
    /** Memo program as transfer ix (wrong program). Accounts: [sender, recipient, memoProg] */
    WRONG_PROGRAM:
        'AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAED253ynD68LT6pZYKXCxW2ZdXbKibEiRnYD8UPXLZkNuPG+nrzvtutOj1l82qryXQxsbvkwtL24OR8pgIDRS9dYQVKU1qZKSEGTSTocWDaOHx8NbXdvJK7geQfqEBBBUSNAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAgIAAQwCAAAAAMqaOwAAAAA=',
    /** SPL TransferChecked (disc=12), 1 USDC. Accounts: [sender, senderATA, recipientATA, splToken, tokenProg] */
    SPL_TRANSFER_CHECKED:
        'AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAIF253ynD68LT6pZYKXCxW2ZdXbKibEiRnYD8UPXLZkNuNicctxGUdrncy4pSly24NYezTDOv+u5FxoObmmc7sj0Oin7ibcocRFXdbqWkgitQB9+5rV3DjkEl8IvjO3CCBTBpuIV/6rgYT7aH9jRhjANdrEOdwa6ztVmKDwAAAAAAEG3fbh12Whk9nL4UbO63msHLSF7V9bN5E6jPWFfv8AqQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQQEAQMCAAoMQEIPAAAAAAAG',
    /** SPL TransferChecked but with system program (wrong). */
    SPL_WRONG_PROGRAM:
        'AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAIF253ynD68LT6pZYKXCxW2ZdXbKibEiRnYD8UPXLZkNuNicctxGUdrncy4pSly24NYezTDOv+u5FxoObmmc7sj0Oin7ibcocRFXdbqWkgitQB9+5rV3DjkEl8IvjO3CCBTAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAGm4hX/quBhPtof2NGGMA12sQ53BrrO1WYoPAAAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQMEAQQCAAoMQEIPAAAAAAAG',
    /** No instructions. Accounts: [sender] */
    EMPTY: 'AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAAB253ynD68LT6pZYKXCxW2ZdXbKibEiRnYD8UPXLZkNuMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=',
    /** Two SOL transfers (no memo). Accounts: [sender, recipient, system] */
    TWO_SOL_TRANSFERS:
        'AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAED253ynD68LT6pZYKXCxW2ZdXbKibEiRnYD8UPXLZkNuPG+nrzvtutOj1l82qryXQxsbvkwtL24OR8pgIDRS9dYQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAgIAAQwCAAAAAGXNHQAAAAACAgABDAIAAAAAypo7AAAAAA==',
};

function createMockRpc(response: any) {
    return {
        getTransaction(_sig: Signature, _opts?: any) {
            return {
                send: vi.fn().mockResolvedValue(response),
            };
        },
    } as any;
}

/** Build a base64-encoded RPC response for SOL transfers. */
function makeSOLResponse(opts: { base64Tx?: string; preBalance?: bigint; postBalance?: bigint; err?: unknown }) {
    return {
        meta: {
            err: opts.err ?? null,
            // Account order: [sender(0), recipient(1), systemProgram(2), ...]
            preBalances: [10_000_000_000n, opts.preBalance ?? 0n, 1n, 1n],
            postBalances: [9_000_000_000n, opts.postBalance ?? 1_000_000_000n, 1n, 1n],
        },
        transaction: [opts.base64Tx ?? TX.SOL_TRANSFER, 'base64'],
    };
}

/** Build a base64-encoded RPC response for SPL transfers. */
function makeSPLResponse(opts: { base64Tx?: string; preAmount?: string; postAmount?: string }) {
    return {
        meta: {
            err: null,
            preBalances: [1n, 1n, 1n, 1n, 1n],
            postBalances: [1n, 1n, 1n, 1n, 1n],
            // Account order: [sender(0), senderATA(1), recipientATA(2), splToken(3), tokenProg(4)]
            preTokenBalances: [
                {
                    accountIndex: 2, // recipientATA
                    uiTokenAmount: { amount: opts.preAmount ?? '0', decimals: 6 },
                },
            ],
            postTokenBalances: [
                {
                    accountIndex: 2, // recipientATA
                    uiTokenAmount: { amount: opts.postAmount ?? '1000000', decimals: 6 },
                },
            ],
        },
        transaction: [opts.base64Tx ?? TX.SPL_TRANSFER_CHECKED, 'base64'],
    };
}

describe('ValidateTransferError', () => {
    it('should create error with correct name', () => {
        const err = new ValidateTransferError('test');
        expect(err.name).toBe('ValidateTransferError');
        expect(err).toBeInstanceOf(Error);
    });
});

describe('validateTransfer', () => {
    describe('input validation', () => {
        it('should throw "amount invalid" for negative amount', async () => {
            const rpc = createMockRpc(null);
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: -1 }),
            ).rejects.toThrow('amount invalid');
        });

        it('should throw "amount invalid" for NaN amount', async () => {
            const rpc = createMockRpc(null);
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: NaN }),
            ).rejects.toThrow('amount invalid');
        });

        it('should throw "amount invalid" for Infinity', async () => {
            const rpc = createMockRpc(null);
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: Infinity }),
            ).rejects.toThrow('amount invalid');
        });

        it('should accept amount of 0', async () => {
            const response = makeSOLResponse({ postBalance: 0n, preBalance: 0n });
            const rpc = createMockRpc(response);
            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 0,
            });
            expect(result).toBe(response);
        });
    });

    describe('SOL transfers', () => {
        it('should validate a valid SOL transfer', async () => {
            const response = makeSOLResponse({ postBalance: 1_000_000_000n });
            const rpc = createMockRpc(response);
            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1,
            });
            expect(result).toBe(response);
        });

        it('should throw "not found" when transaction is null', async () => {
            const rpc = createMockRpc(null);
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: 1 }),
            ).rejects.toThrow('not found');
        });

        it('should throw "missing meta" when meta is null', async () => {
            const rpc = createMockRpc({
                meta: null,
                transaction: [TX.SOL_TRANSFER, 'base64'],
            });
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: 1 }),
            ).rejects.toThrow('missing meta');
        });

        it('should throw ValidateTransferError when transaction has an error', async () => {
            const response = makeSOLResponse({ err: { InstructionError: [0, 'Custom'] } });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: 1 }),
            ).rejects.toThrow(ValidateTransferError);
        });

        it('should throw "amount not transferred" when insufficient amount', async () => {
            const response = makeSOLResponse({ postBalance: 100n });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: 1 }),
            ).rejects.toThrow('amount not transferred');
        });

        it('should validate transfer with correct references', async () => {
            const response = makeSOLResponse({
                base64Tx: TX.SOL_TRANSFER_WITH_REF,
                postBalance: 1_000_000_000n,
            });
            const rpc = createMockRpc(response);
            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1,
                reference: ADDRESSES.reference,
            });
            expect(result).toBe(response);
        });

        it('should validate transfer with reference passed as array', async () => {
            const response = makeSOLResponse({
                base64Tx: TX.SOL_TRANSFER_WITH_REF,
                postBalance: 1_000_000_000n,
            });
            const rpc = createMockRpc(response);
            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1,
                reference: [ADDRESSES.reference],
            });
            expect(result).toBe(response);
        });

        it('should throw "invalid references" when reference count mismatch', async () => {
            // TX has no reference accounts but we expect one
            const response = makeSOLResponse({ postBalance: 1_000_000_000n });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    reference: ADDRESSES.reference,
                }),
            ).rejects.toThrow('invalid references');
        });

        it('should throw "invalid reference" when address does not match', async () => {
            // TX has reference key but we expect a different one
            const response = makeSOLResponse({
                base64Tx: TX.SOL_TRANSFER_WITH_REF,
                postBalance: 1_000_000_000n,
            });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    reference: ADDRESSES.sender, // wrong address
                }),
            ).rejects.toThrow('invalid reference 0');
        });

        it('should throw when the last instruction is not a system transfer', async () => {
            // WRONG_PROGRAM has memo program as the transfer instruction's program
            const response = makeSOLResponse({ base64Tx: TX.WRONG_PROGRAM, postBalance: 1_000_000_000n });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: 1 }),
            ).rejects.toThrow(ValidateTransferError);
        });

        it('should throw "missing instruction" for empty transaction', async () => {
            const response = makeSOLResponse({ base64Tx: TX.EMPTY });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, { recipient: ADDRESSES.recipient, amount: 1 }),
            ).rejects.toThrow('missing instruction');
        });
    });

    describe('SPL transfers', () => {
        it('should validate a valid SPL TransferChecked', async () => {
            const response = makeSPLResponse({});
            const rpc = createMockRpc(response);
            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1,
                splToken: ADDRESSES.splToken,
            });
            expect(result).toBe(response);
        });

        it('should throw "invalid transfer" when the program is not a token program', async () => {
            const response = makeSPLResponse({ base64Tx: TX.SPL_WRONG_PROGRAM });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('invalid transfer');
        });

        it('should throw "amount not transferred" for insufficient SPL amount', async () => {
            const response = makeSPLResponse({ postAmount: '100' }); // only 100 base units
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1, // 1 token = 1_000_000 base units
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('amount not transferred');
        });

        it('should throw "invalid transfer" when destination ATA does not match', async () => {
            // Mock findAssociatedTokenPda to return a different ATA
            const { findAssociatedTokenPda } = await import('@solana-program/token');
            (findAssociatedTokenPda as ReturnType<typeof vi.fn>).mockResolvedValueOnce([
                ADDRESSES.sender as Address, // wrong ATA
                255,
            ]);
            const response = makeSPLResponse({});
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('invalid transfer');
        });

        it('should throw "invalid transfer" when mint does not match splToken', async () => {
            const response = makeSPLResponse({});
            const rpc = createMockRpc(response);
            // Pass a different splToken — the tx's mint account won't match
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    splToken: ADDRESSES.sender, // wrong mint
                }),
            ).rejects.toThrow('invalid transfer');
        });
    });

    describe('memo validation', () => {
        it('should validate valid memo', async () => {
            const response = {
                meta: {
                    err: null,
                    preBalances: [10_000_000_000n, 0n, 1n, 1n],
                    postBalances: [9_000_000_000n, 1_000_000_000n, 1n, 1n],
                },
                transaction: [TX.SOL_TRANSFER_WITH_MEMO, 'base64'],
            };
            const rpc = createMockRpc(response);
            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1,
                memo: 'test',
            });
            expect(result).toBe(response);
        });

        it('should throw "missing memo instruction" when memo expected but not present', async () => {
            // SOL_TRANSFER has only 1 instruction, so after popping transfer there's nothing left for memo
            const response = makeSOLResponse({ postBalance: 1_000_000_000n });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    memo: 'test',
                }),
            ).rejects.toThrow('missing memo instruction');
        });

        it('should throw "invalid memo" on wrong memo content', async () => {
            const response = {
                meta: {
                    err: null,
                    preBalances: [10_000_000_000n, 0n, 1n, 1n],
                    postBalances: [9_000_000_000n, 1_000_000_000n, 1n, 1n],
                },
                transaction: [TX.SOL_TRANSFER_WITH_MEMO, 'base64'],
            };
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    memo: 'wrong memo',
                }),
            ).rejects.toThrow('invalid memo');
        });

        it('should throw "invalid memo program" when second-to-last instruction is not memo program', async () => {
            // TWO_SOL_TRANSFERS has 2 system transfer ixs — second-to-last is system, not memo
            const response = makeSOLResponse({
                base64Tx: TX.TWO_SOL_TRANSFERS,
                postBalance: 1_000_000_000n,
            });
            const rpc = createMockRpc(response);
            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    memo: 'test',
                }),
            ).rejects.toThrow('invalid memo program');
        });
    });
});
