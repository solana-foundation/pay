import { AccountRole, type Address, address, type TransactionSigner } from '@solana/kit';
import { SYSTEM_PROGRAM_ADDRESS } from '@solana-program/system';
import { TOKEN_PROGRAM_ADDRESS } from '@solana-program/token';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { MEMO_PROGRAM_ADDRESS, TOKEN_2022_PROGRAM_ADDRESS } from '../src/constants.js';
import { createTransfer, CreateTransferError } from '../src/index.js';

const mockedFetchMint = vi.fn();
const mockedFetchToken = vi.fn();
const mockedFindAssociatedTokenPda = vi.fn();

vi.mock('@solana-program/token', async () => {
    return {
        TOKEN_PROGRAM_ADDRESS: 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA',
        fetchMint: (...args: unknown[]) => mockedFetchMint(...args),
        fetchToken: (...args: unknown[]) => mockedFetchToken(...args),
        findAssociatedTokenPda: (...args: unknown[]) => mockedFindAssociatedTokenPda(...args),
        getTransferCheckedInstruction: vi.fn().mockImplementation((input: any, config?: any) => {
            return {
                programAddress: config?.programAddress ?? 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA',
                accounts: [
                    { address: input.source, role: 1 },
                    { address: input.mint, role: 0 },
                    { address: input.destination, role: 1 },
                    { address: input.authority?.address ?? input.authority, role: 2 },
                ],
                data: new Uint8Array([12]), // TransferChecked discriminator
            };
        }),
    };
});

const TOKEN_2022_PROGRAM = TOKEN_2022_PROGRAM_ADDRESS;

const TEST_AMOUNTS = {
    ONE_TOKEN: 1,
    TWO_TOKENS: 2,
} as const;

const ADDRESSES = {
    sender: address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa'),
    recipient: address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'),
    splToken: address('9aE476sH92Vz7DMPyq5WLPkrKWivxeuTKEFKd2sZZcde'),
    senderATA: address('2jDmYQMRCBnXUQeFRvQABcU6hLcvjVTdG7AoHravxWJX'),
    recipientATA: address('GfC73miMwXBoRYDn7gvEZVbhM7n6SUHxJb4LdBz2Mfp6'),
};

function createMockSigner(addr: Address): TransactionSigner {
    return {
        address: addr,
        signTransactions: vi.fn(),
    } as unknown as TransactionSigner;
}

function createMockRpc(accountInfoResponses: Map<string, any>) {
    return {
        getAccountInfo(addr: Address, _config?: unknown) {
            return {
                send: vi.fn().mockResolvedValue({
                    value: accountInfoResponses.get(addr) ?? null,
                }),
            };
        },
    } as any;
}

function makeAccountInfo(owner: Address, lamports: bigint | number, executable = false) {
    return {
        owner,
        executable,
        lamports: BigInt(lamports),
        data: new Uint8Array(0),
        rentEpoch: 0n,
    };
}

function makeMintData(decimals = 6, isInitialized = true) {
    return {
        address: ADDRESSES.splToken,
        data: {
            mintAuthority: address('11111111111111111111111111111112'),
            supply: 1_000_000_000n,
            decimals,
            isInitialized,
            freezeAuthority: null,
        },
        executable: false,
        lamports: 0n,
        programAddress: TOKEN_PROGRAM_ADDRESS,
    } as any;
}

function makeTokenAccountData(opts: { addr: Address; mint: Address; owner: Address; amount?: bigint; state?: number }) {
    return {
        address: opts.addr,
        data: {
            mint: opts.mint,
            owner: opts.owner,
            amount: opts.amount ?? 0n,
            delegate: null,
            delegateAmount: 0n,
            state: opts.state ?? 1,
            isNative: null,
            closeAuthority: null,
        },
        executable: false,
        lamports: 0n,
        programAddress: TOKEN_PROGRAM_ADDRESS,
    } as any;
}

describe('CreateTransferError', () => {
    it('should create error with correct name and message', () => {
        const error = new CreateTransferError('Test error message');
        expect(error.name).toBe('CreateTransferError');
        expect(error.message).toBe('Test error message');
        expect(error).toBeInstanceOf(Error);
    });
});

describe('createTransfer', () => {
    let sender: TransactionSigner;

    beforeEach(() => {
        sender = createMockSigner(ADDRESSES.sender);
        vi.clearAllMocks();
    });

    describe('SOL transfers', () => {
        it('should create valid SOL transfer with correct instruction shape', async () => {
            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 1_000_000_000)],
                [ADDRESSES.recipient, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 0)],
            ]);
            const rpc = createMockRpc(accounts);

            const instructions = await createTransfer(rpc, sender, {
                recipient: ADDRESSES.recipient,
                amount: TEST_AMOUNTS.ONE_TOKEN,
            });

            expect(Array.isArray(instructions)).toBe(true);
            expect(instructions.length).toBe(1);

            // Instruction shape assertions
            const ix = instructions[0];
            expect(ix.programAddress).toBe(SYSTEM_PROGRAM_ADDRESS);
            expect(ix.data).toBeInstanceOf(Uint8Array);
            expect(ix.accounts).toBeDefined();
            // Should have source and destination accounts
            const accountAddresses = ix.accounts!.map((a: any) => a.address);
            expect(accountAddresses).toContain(ADDRESSES.sender);
            expect(accountAddresses).toContain(ADDRESSES.recipient);
        });

        it('should create SOL transfer with memo — correct program addresses', async () => {
            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 1_000_000_000)],
                [ADDRESSES.recipient, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 0)],
            ]);
            const rpc = createMockRpc(accounts);

            const instructions = await createTransfer(rpc, sender, {
                recipient: ADDRESSES.recipient,
                amount: TEST_AMOUNTS.ONE_TOKEN,
                memo: 'test memo',
            });

            expect(instructions.length).toBe(2);
            // First instruction is memo
            expect(instructions[0].programAddress).toBe(MEMO_PROGRAM_ADDRESS);
            expect(instructions[0].data).toBeInstanceOf(Uint8Array);
            // Second instruction is transfer
            expect(instructions[1].programAddress).toBe(SYSTEM_PROGRAM_ADDRESS);
        });

        it('should create SOL transfer with reference — reference has AccountRole.READONLY', async () => {
            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 1_000_000_000)],
                [ADDRESSES.recipient, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 0)],
            ]);
            const rpc = createMockRpc(accounts);
            const ref = address('82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny');

            const instructions = await createTransfer(rpc, sender, {
                recipient: ADDRESSES.recipient,
                amount: TEST_AMOUNTS.ONE_TOKEN,
                reference: ref,
            });

            expect(instructions.length).toBe(1);
            const transferIx = instructions[0];
            const refAccount = transferIx.accounts!.find((a: any) => a.address === ref);
            expect(refAccount).toBeDefined();
            expect(refAccount!.role).toBe(AccountRole.READONLY);
        });

        it('should handle multiple references', async () => {
            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 1_000_000_000)],
                [ADDRESSES.recipient, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 0)],
            ]);
            const rpc = createMockRpc(accounts);
            const ref1 = address('82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny');
            const ref2 = address('9aE476sH92Vz7DMPyq5WLPkrKWivxeuTKEFKd2sZZcde');

            const instructions = await createTransfer(rpc, sender, {
                recipient: ADDRESSES.recipient,
                amount: TEST_AMOUNTS.ONE_TOKEN,
                reference: [ref1, ref2],
            });

            expect(instructions.length).toBe(1);
            const accts = instructions[0].accounts!;
            expect(accts.find((a: any) => a.address === ref1)).toBeDefined();
            expect(accts.find((a: any) => a.address === ref2)).toBeDefined();
            // Both should be READONLY
            expect(accts.find((a: any) => a.address === ref1)!.role).toBe(AccountRole.READONLY);
            expect(accts.find((a: any) => a.address === ref2)!.role).toBe(AccountRole.READONLY);
        });

        it('should throw when sender account does not exist', async () => {
            const accounts = new Map<string, any>();
            const rpc = createMockRpc(accounts);

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                }),
            ).rejects.toThrow(CreateTransferError);
        });

        it('should throw when recipient account does not exist', async () => {
            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 1_000_000_000)],
            ]);
            const rpc = createMockRpc(accounts);

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                }),
            ).rejects.toThrow(CreateTransferError);
        });

        it('should throw when sender owner is invalid', async () => {
            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(ADDRESSES.splToken, 1_000_000_000)],
                [ADDRESSES.recipient, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 0)],
            ]);
            const rpc = createMockRpc(accounts);

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                }),
            ).rejects.toThrow('sender owner invalid');
        });

        it('should throw when sender is executable', async () => {
            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 1_000_000_000, true)],
                [ADDRESSES.recipient, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 0)],
            ]);
            const rpc = createMockRpc(accounts);

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                }),
            ).rejects.toThrow('sender executable');
        });

        it('should throw on insufficient funds', async () => {
            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 100)], // only 100 lamports
                [ADDRESSES.recipient, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 0)],
            ]);
            const rpc = createMockRpc(accounts);

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN, // 1 SOL = 1B lamports
                }),
            ).rejects.toThrow('insufficient funds');
        });
    });

    describe('SPL Token transfers', () => {
        function setupTokenMocks(overrides?: {
            mintInitialized?: boolean;
            senderState?: number;
            senderAmount?: bigint;
            recipientState?: number;
            tokenProgram?: Address;
        }) {
            const tokenProgram = overrides?.tokenProgram ?? (TOKEN_PROGRAM_ADDRESS as Address);

            const accounts = new Map<string, any>([
                [ADDRESSES.sender, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 1_000_000_000)],
                [ADDRESSES.recipient, makeAccountInfo(SYSTEM_PROGRAM_ADDRESS, 0)],
                [ADDRESSES.splToken, makeAccountInfo(tokenProgram, 0)],
            ]);
            const rpc = createMockRpc(accounts);

            mockedFetchMint.mockReset();
            mockedFetchToken.mockReset();
            mockedFindAssociatedTokenPda.mockReset();

            mockedFetchMint.mockResolvedValue(makeMintData(6, overrides?.mintInitialized ?? true));

            mockedFindAssociatedTokenPda.mockImplementation(async (args: any) => {
                if (args.owner === ADDRESSES.sender) return [ADDRESSES.senderATA, 255] as any;
                if (args.owner === ADDRESSES.recipient) return [ADDRESSES.recipientATA, 255] as any;
                return [ADDRESSES.senderATA, 255] as any;
            });

            const senderTokenData = makeTokenAccountData({
                addr: ADDRESSES.senderATA,
                mint: ADDRESSES.splToken,
                owner: ADDRESSES.sender,
                amount: overrides?.senderAmount ?? 1_000_000n,
                state: overrides?.senderState ?? 1,
            });
            const recipientTokenData = makeTokenAccountData({
                addr: ADDRESSES.recipientATA,
                mint: ADDRESSES.splToken,
                owner: ADDRESSES.recipient,
                amount: 0n,
                state: overrides?.recipientState ?? 1,
            });

            mockedFetchToken.mockImplementation(async (_rpc: any, addr: any) => {
                if (addr === ADDRESSES.senderATA) return senderTokenData;
                if (addr === ADDRESSES.recipientATA) return recipientTokenData;
                return senderTokenData;
            });

            return rpc;
        }

        it('should create valid SPL token transfer with correct shape', async () => {
            const rpc = setupTokenMocks();

            const instructions = await createTransfer(rpc, sender, {
                recipient: ADDRESSES.recipient,
                amount: TEST_AMOUNTS.ONE_TOKEN,
                splToken: ADDRESSES.splToken,
            });

            expect(Array.isArray(instructions)).toBe(true);
            expect(instructions.length).toBe(1);
            expect(mockedFetchMint).toHaveBeenCalledTimes(1);
            expect(mockedFetchToken).toHaveBeenCalledTimes(2);

            // Shape assertion: SPL transfer should use token program
            const ix = instructions[0];
            expect(ix.programAddress).toBe(TOKEN_PROGRAM_ADDRESS);
            expect(ix.data).toBeInstanceOf(Uint8Array);
        });

        it('should use Token-2022 program address when token is owned by Token-2022', async () => {
            const rpc = setupTokenMocks({ tokenProgram: TOKEN_2022_PROGRAM });

            const instructions = await createTransfer(rpc, sender, {
                recipient: ADDRESSES.recipient,
                amount: TEST_AMOUNTS.ONE_TOKEN,
                splToken: ADDRESSES.splToken,
            });

            expect(instructions.length).toBe(1);
            expect(instructions[0].programAddress).toBe(TOKEN_2022_PROGRAM);
        });

        it('should throw when mint is not initialized', async () => {
            const rpc = setupTokenMocks({ mintInitialized: false });

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('mint not initialized');
        });

        it('should throw when sender token account is not initialized', async () => {
            const rpc = setupTokenMocks({ senderState: 0 });

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('sender not initialized');
        });

        it('should throw when sender token account is frozen', async () => {
            const rpc = setupTokenMocks({ senderState: 2 });

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('sender frozen');
        });

        it('should throw when recipient token account is not initialized', async () => {
            const rpc = setupTokenMocks({ recipientState: 0 });

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('recipient not initialized');
        });

        it('should throw when recipient token account is frozen', async () => {
            const rpc = setupTokenMocks({ recipientState: 2 });

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.ONE_TOKEN,
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('recipient frozen');
        });

        it('should throw when sender has insufficient token balance', async () => {
            const rpc = setupTokenMocks({ senderAmount: 1n });

            await expect(
                createTransfer(rpc, sender, {
                    recipient: ADDRESSES.recipient,
                    amount: TEST_AMOUNTS.TWO_TOKENS,
                    splToken: ADDRESSES.splToken,
                }),
            ).rejects.toThrow('insufficient funds');
        });
    });
});
