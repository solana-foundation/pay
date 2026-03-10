import { type Address, address, type Signature } from '@solana/kit';
import { MEMO_PROGRAM_ADDRESS } from '@solana-program/memo';
import { SYSTEM_PROGRAM_ADDRESS } from '@solana-program/system';
import { describe, expect, it, vi } from 'vitest';

import { validateTransfer, ValidateTransferError } from '../src/index.js';

// Mock findAssociatedTokenPda
vi.mock('@solana-program/token', () => ({
    TOKEN_PROGRAM_ADDRESS: 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA',
    findAssociatedTokenPda: vi.fn().mockResolvedValue(['GfC73miMwXBoRYDn7gvEZVbhM7n6SUHxJb4LdBz2Mfp6' as Address, 255]),
}));
const SIGNATURE = '5UfDuX7hXbDBZpHnSEFMwBN6JdANTF54fGVz9Kp1fZBNTmRmEiGP' as Signature;

const ADDRESSES = {
    sender: address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa'),
    recipient: address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'),
    reference: address('82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny'),
    recipientATA: address('GfC73miMwXBoRYDn7gvEZVbhM7n6SUHxJb4LdBz2Mfp6'),
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

function makeSOLTransferResponse(opts: {
    preBalance?: bigint;
    postBalance?: bigint;
    err?: unknown;
    extraAccountKeys?: Address[];
    extraInstructionAccounts?: number[];
    memoData?: string;
    memoAccounts?: number[];
}) {
    const accountKeys: Address[] = [
        ADDRESSES.sender, // 0
        ADDRESSES.recipient, // 1
        SYSTEM_PROGRAM_ADDRESS, // 2
        ...(opts.extraAccountKeys ?? []),
    ];

    const transferInstruction = {
        programIdIndex: 2, // system program
        accounts: [0, 1, ...(opts.extraInstructionAccounts ?? [])],
        data: '3Bxs4ThwQbE4vyj3', // base58 encoded transfer data (arbitrary valid)
    };

    const instructions = [];
    if (opts.memoData !== undefined) {
        accountKeys.push(MEMO_PROGRAM_ADDRESS);
        instructions.push({
            programIdIndex: accountKeys.length - 1,
            accounts: opts.memoAccounts ?? [],
            data: opts.memoData,
        });
    }
    instructions.push(transferInstruction);

    return {
        meta: {
            err: opts.err ?? null,
            preBalances: [10_000_000_000n, opts.preBalance ?? 0n, 1n],
            postBalances: [9_000_000_000n, opts.postBalance ?? 1_000_000_000n, 1n],
        },
        transaction: {
            message: { accountKeys, instructions },
            signatures: ['sig1'],
        },
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
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: -1,
                }),
            ).rejects.toThrow('amount invalid');
        });

        it('should throw "amount invalid" for NaN amount', async () => {
            const rpc = createMockRpc(null);

            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: NaN,
                }),
            ).rejects.toThrow('amount invalid');
        });
    });

    describe('SOL transfers', () => {
        it('should validate a valid SOL transfer', async () => {
            const response = makeSOLTransferResponse({ postBalance: 1_000_000_000n });
            const rpc = createMockRpc(response);

            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1, // 1 SOL
            });

            expect(result).toBe(response);
        });

        it('should throw "not found" when transaction is null', async () => {
            const rpc = createMockRpc(null);

            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                }),
            ).rejects.toThrow('not found');
        });

        it('should throw "missing meta" when meta is null', async () => {
            const rpc = createMockRpc({
                meta: null,
                transaction: { message: { accountKeys: [], instructions: [] }, signatures: [] },
            });

            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                }),
            ).rejects.toThrow('missing meta');
        });

        it('should throw ValidateTransferError when transaction has an error', async () => {
            const response = makeSOLTransferResponse({ err: { InstructionError: [0, 'Custom'] } });
            const rpc = createMockRpc(response);

            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                }),
            ).rejects.toThrow(ValidateTransferError);
        });

        it('should throw "amount not transferred" when insufficient amount', async () => {
            const response = makeSOLTransferResponse({ postBalance: 100n }); // only 100 lamports
            const rpc = createMockRpc(response);

            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1, // 1 SOL
                }),
            ).rejects.toThrow('amount not transferred');
        });

        it('should validate transfer with correct references', async () => {
            const response = makeSOLTransferResponse({
                postBalance: 1_000_000_000n,
                extraAccountKeys: [ADDRESSES.reference],
                extraInstructionAccounts: [3], // index of reference in accountKeys
            });
            const rpc = createMockRpc(response);

            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1,
                reference: ADDRESSES.reference,
            });

            expect(result).toBe(response);
        });

        it('should throw "invalid references" when reference count mismatch', async () => {
            const response = makeSOLTransferResponse({ postBalance: 1_000_000_000n });
            const rpc = createMockRpc(response);

            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    reference: ADDRESSES.reference, // expects reference but none in tx
                }),
            ).rejects.toThrow('invalid references');
        });
    });

    describe('memo validation', () => {
        it('should validate valid memo', async () => {
            // "test" in UTF-8 bytes encoded as base58 = "3yZe7d"
            const response = makeSOLTransferResponse({
                postBalance: 1_000_000_000n,
                memoData: '3yZe7d', // base58 of "test"
            });
            const rpc = createMockRpc(response);

            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1,
                memo: 'test',
            });

            expect(result).toBe(response);
        });

        it('should validate memo with signer accounts', async () => {
            // Memo program allows signer accounts; createTransfer adds signers: [sender]
            const response = makeSOLTransferResponse({
                postBalance: 1_000_000_000n,
                memoData: '3yZe7d', // base58 of "test"
                memoAccounts: [0], // sender as signer account
            });
            const rpc = createMockRpc(response);

            const result = await validateTransfer(rpc, SIGNATURE, {
                recipient: ADDRESSES.recipient,
                amount: 1,
                memo: 'test',
            });

            expect(result).toBe(response);
        });

        it('should throw "missing memo instruction" when memo expected but not present', async () => {
            const response = makeSOLTransferResponse({ postBalance: 1_000_000_000n });
            // Only transfer instruction, no memo
            response.transaction.message.instructions = [
                response.transaction.message.instructions[0], // only transfer
            ];
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
            const response = makeSOLTransferResponse({
                postBalance: 1_000_000_000n,
                memoData: '3yZe7d', // base58 of "test"
            });
            const rpc = createMockRpc(response);

            await expect(
                validateTransfer(rpc, SIGNATURE, {
                    recipient: ADDRESSES.recipient,
                    amount: 1,
                    memo: 'wrong memo',
                }),
            ).rejects.toThrow('invalid memo');
        });
    });
});
