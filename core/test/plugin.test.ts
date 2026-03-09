import { address, type Address, type TransactionSigner } from '@solana/kit';
import { describe, it, expect, vi } from 'vitest';
import { solanaPay } from '../src/plugin.js';

function createMockSigner(addr: Address): TransactionSigner {
    return {
        address: addr,
        signTransactions: vi.fn(),
    } as unknown as TransactionSigner;
}

function createMockClient(payer?: TransactionSigner) {
    return {
        rpc: {} as any,
        ...(payer ? { payer } : {}),
    };
}

describe('solanaPay plugin', () => {
    describe('installation', () => {
        it('should add pay namespace to client', () => {
            const client = createMockClient();
            const plugin = solanaPay();
            const extended = plugin(client);

            expect(extended.pay).toBeDefined();
            expect(typeof extended.pay.createTransfer).toBe('function');
            expect(typeof extended.pay.encodeURL).toBe('function');
            expect(typeof extended.pay.parseURL).toBe('function');
            expect(typeof extended.pay.createQR).toBe('function');
            expect(typeof extended.pay.createQROptions).toBe('function');
            expect(typeof extended.pay.findReference).toBe('function');
            expect(typeof extended.pay.validateTransfer).toBe('function');
            expect(typeof extended.pay.fetchTransaction).toBe('function');
        });

        it('should preserve existing client properties', () => {
            const client = { rpc: {} as any, customProp: 'hello' };
            const plugin = solanaPay();
            const extended = plugin(client);

            expect(extended.customProp).toBe('hello');
            expect(extended.rpc).toBe(client.rpc);
        });

        it('should return a frozen object', () => {
            const client = createMockClient();
            const plugin = solanaPay();
            const extended = plugin(client);

            expect(Object.isFrozen(extended)).toBe(true);
        });
    });

    describe('encodeURL', () => {
        it('should encode a transfer request URL', () => {
            const client = createMockClient();
            const extended = solanaPay()(client);

            const recipient = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
            const url = extended.pay.encodeURL({
                recipient,
                amount: 1,
            });

            expect(url).toBeInstanceOf(URL);
            expect(url.protocol).toBe('solana:');
            expect(url.pathname).toBe(recipient);
        });

        it('should encode a transaction request URL', () => {
            const client = createMockClient();
            const extended = solanaPay()(client);

            const link = 'https://example.com/pay';
            const url = extended.pay.encodeURL({ link: new URL(link) });

            expect(url).toBeInstanceOf(URL);
            expect(url.protocol).toBe('solana:');
        });
    });

    describe('parseURL', () => {
        it('should parse a transfer request URL', () => {
            const client = createMockClient();
            const extended = solanaPay()(client);

            const recipient = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
            const url = `solana:${recipient}?amount=1`;
            const parsed = extended.pay.parseURL(url);

            expect(parsed.recipient).toBe(recipient);
        });

        it('should roundtrip encode → parse', () => {
            const client = createMockClient();
            const extended = solanaPay()(client);

            const recipient = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
            const amount = 2.5;
            const url = extended.pay.encodeURL({ recipient, amount });
            const parsed = extended.pay.parseURL(url);

            expect(parsed.recipient).toBe(recipient);
            expect('amount' in parsed && parsed.amount).toBe(2.5);
        });
    });

    describe('createTransfer', () => {
        it('should throw when no sender or payer is available', async () => {
            const client = createMockClient(); // no payer
            const extended = solanaPay()(client);

            await expect(
                extended.pay.createTransfer({
                    recipient: address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'),
                    amount: 1,
                }),
            ).rejects.toThrow('requires a sender or client.payer');
        });

        it('should use client.payer when no explicit sender is provided', async () => {
            const payer = createMockSigner(address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa'));
            const mockRpc = {
                getAccountInfo: vi.fn().mockReturnValue({
                    send: vi.fn().mockResolvedValue({
                        value: {
                            owner: address('11111111111111111111111111111111'),
                            executable: false,
                            lamports: 1_000_000_000n,
                            data: new Uint8Array(0),
                            rentEpoch: 0n,
                        },
                    }),
                }),
            };
            const recipientAddr = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
            mockRpc.getAccountInfo.mockImplementation((addr: any) => ({
                send: vi.fn().mockResolvedValue({
                    value: {
                        owner: address('11111111111111111111111111111111'),
                        executable: false,
                        lamports: addr === payer.address ? 1_000_000_000n : 0n,
                        data: new Uint8Array(0),
                        rentEpoch: 0n,
                    },
                }),
            }));

            const client = { rpc: mockRpc as any, payer };
            const extended = solanaPay()(client);

            const instructions = await extended.pay.createTransfer({
                recipient: recipientAddr,
                amount: 1,
            });

            expect(Array.isArray(instructions)).toBe(true);
            expect(instructions.length).toBeGreaterThan(0);
        });

        it('should use explicit sender over client.payer', async () => {
            const payer = createMockSigner(address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa'));
            const sender = createMockSigner(address('82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny'));

            const mockRpc = {
                getAccountInfo: vi.fn().mockImplementation(() => ({
                    send: vi.fn().mockResolvedValue({
                        value: {
                            owner: address('11111111111111111111111111111111'),
                            executable: false,
                            lamports: 1_000_000_000n,
                            data: new Uint8Array(0),
                            rentEpoch: 0n,
                        },
                    }),
                })),
            };

            const client = { rpc: mockRpc as any, payer };
            const extended = solanaPay()(client);

            const instructions = await extended.pay.createTransfer(
                {
                    recipient: address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'),
                    amount: 1,
                },
                sender,
            );

            expect(Array.isArray(instructions)).toBe(true);
            const transferIx = instructions[0] as any;
            const senderAccount = transferIx.accounts?.find((a: any) => a.address === sender.address);
            expect(senderAccount).toBeDefined();
        });
    });
});
