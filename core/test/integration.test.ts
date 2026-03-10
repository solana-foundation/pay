import type { GetSignaturesForAddressApi, GetTransactionApi, Rpc } from '@solana/kit';
import { generateKeyPairSigner, lamports, TransactionSigner } from '@solana/kit';
import { createClient } from '@solana/kit-client-litesvm';
import {
    findAssociatedTokenPda,
    getCreateAssociatedTokenIdempotentInstructionAsync,
    TOKEN_PROGRAM_ADDRESS,
    tokenProgram,
} from '@solana-program/token';
import { beforeAll, describe, expect, it } from 'vitest';

import { encodeURL, parseURL, solanaPayMerchant, solanaPayWallet } from '../src/index.js';

/**
 *
 * Stub unsupported RPC methods so the client satisfies SolanaPayCompatibleClient.
 * LiteSVM's RPC doesn't include getSignaturesForAddress/getTransaction,
 * but integration tests only exercise createTransfer which doesn't need them.
 * This allows us to maintain type safety while still allowing us to run the tests.
 */
function withUnsupportedRpcStubs() {
    return <T extends { rpc: Rpc<object> }>(client: T) => ({
        ...client,
        rpc: new Proxy(client.rpc, {
            get(target, prop, receiver) {
                if (prop === 'getSignaturesForAddress' || prop === 'getTransaction') {
                    return () => {
                        throw new Error(`${String(prop)} is not supported by LiteSVM`);
                    };
                }
                return Reflect.get(target, prop, receiver);
            },
        }) as Rpc<GetSignaturesForAddressApi & GetTransactionApi> & T['rpc'],
    });
}

async function createTestClient() {
    const client = await createClient()
        .use(withUnsupportedRpcStubs())
        .use(solanaPayMerchant())
        .use(solanaPayWallet())
        .use(tokenProgram());
    return client;
}

type TestClient = Awaited<ReturnType<typeof createTestClient>>;

describe('Integration: SOL transfers', () => {
    let client: TestClient;
    let payer: TransactionSigner;
    let recipient: TransactionSigner;

    beforeAll(async () => {
        client = await createTestClient();
        payer = client.payer;
        client.svm.airdrop(payer.address, lamports(100_000_000_000n));
        recipient = await generateKeyPairSigner();
        client.svm.airdrop(recipient.address, lamports(1n));
    });

    it('should transfer 1 SOL', async () => {
        const instructions = await client.pay.createTransfer({ recipient: recipient.address, amount: 1 });
        await client.sendTransaction(instructions);

        expect(client.svm.getBalance(recipient.address)).toBe(lamports(1_000_000_001n));
    });

    it('should transfer 0.5 SOL (fractional)', async () => {
        const r = await generateKeyPairSigner();
        client.svm.airdrop(r.address, lamports(1n));

        const instructions = await client.pay.createTransfer({ recipient: r.address, amount: 0.5 });
        await client.sendTransaction(instructions);

        expect(client.svm.getBalance(r.address)).toBe(lamports(500_000_001n));
    });

    it('should transfer 1 lamport (0.000000001 SOL)', async () => {
        const r = await generateKeyPairSigner();
        client.svm.airdrop(r.address, lamports(1n));

        const instructions = await client.pay.createTransfer({ recipient: r.address, amount: 0.000000001 });
        await client.sendTransaction(instructions);

        expect(client.svm.getBalance(r.address)).toBe(lamports(2n));
    });

    it('should transfer with memo', async () => {
        const r = await generateKeyPairSigner();
        client.svm.airdrop(r.address, lamports(1n));

        const instructions = await client.pay.createTransfer({
            recipient: r.address,
            amount: 0.001,
            memo: 'order-123',
        });
        await client.sendTransaction(instructions);

        expect(client.svm.getBalance(r.address)).toBe(lamports(1_000_001n));
    });

    it('should transfer with reference', async () => {
        const r = await generateKeyPairSigner();
        client.svm.airdrop(r.address, lamports(1n));
        const reference = (await generateKeyPairSigner()).address;

        const instructions = await client.pay.createTransfer({ recipient: r.address, amount: 0.001, reference });
        await client.sendTransaction(instructions);

        expect(client.svm.getBalance(r.address)).toBe(lamports(1_000_001n));
    });

    it('should throw on insufficient funds', async () => {
        const poorSender = await generateKeyPairSigner();
        client.svm.airdrop(poorSender.address, lamports(100n));
        const r = await generateKeyPairSigner();
        client.svm.airdrop(r.address, lamports(1n));

        await expect(client.pay.createTransfer({ recipient: r.address, amount: 1 }, poorSender)).rejects.toThrow(
            'insufficient funds',
        );
    });
});

describe('Integration: URL round-trip', () => {
    let client: TestClient;

    beforeAll(async () => {
        client = await createTestClient();
    });

    it('should encodeURL → parseURL → createTransfer → send', async () => {
        const r = await generateKeyPairSigner();
        client.svm.airdrop(r.address, lamports(1n));

        const url = encodeURL({ recipient: r.address, amount: 0.25 });
        const parsed = parseURL(url);
        if (!('recipient' in parsed)) throw new Error('expected transfer request URL');

        const instructions = await client.pay.createTransfer({
            recipient: parsed.recipient,
            amount: parsed.amount!,
        });
        await client.sendTransaction(instructions);

        expect(client.svm.getBalance(r.address)).toBe(lamports(250_000_001n));
    });
});

describe('Integration: SPL token transfers', () => {
    let client: TestClient;
    let mint: TransactionSigner;
    let recipient: TransactionSigner;
    const DECIMALS = 6;
    const MINT_AMOUNT = 1_000_000n; // 1 token in base units

    beforeAll(async () => {
        client = await createTestClient();
        const payer = client.payer;
        recipient = await generateKeyPairSigner();
        client.svm.airdrop(recipient.address, lamports(10_000_000n));
        mint = await generateKeyPairSigner();
        await client.token.instructions
            .createMint({
                payer,
                newMint: mint,
                decimals: DECIMALS,
                mintAuthority: payer.address,
            })
            .sendTransaction();
        await client.token.instructions
            .mintToATA({
                payer,
                mint: mint.address,
                amount: MINT_AMOUNT,
                decimals: DECIMALS,
                mintAuthority: payer,
                owner: payer.address,
            })
            .sendTransaction();
        await client.token.instructions
            .mintToATA({
                payer,
                mint: mint.address,
                amount: 0n,
                decimals: DECIMALS,
                mintAuthority: payer,
                owner: recipient.address,
            })
            .sendTransaction();
    });

    it('should transfer SPL tokens', async () => {
        const instructions = await client.pay.createTransfer({
            recipient: recipient.address,
            amount: 1,
            splToken: mint.address,
        });
        await client.sendTransaction(instructions);

        const [recipientATA] = await findAssociatedTokenPda({
            owner: recipient.address,
            tokenProgram: TOKEN_PROGRAM_ADDRESS,
            mint: mint.address,
        });
        const account = await client.token.accounts.token.fetch(recipientATA);
        expect(account.data.amount).toBe(MINT_AMOUNT);
    });

    it('should throw on insufficient SPL token balance', async () => {
        const poorSender = await generateKeyPairSigner();
        client.svm.airdrop(poorSender.address, lamports(1_000_000_000n));

        const createATAIx = await getCreateAssociatedTokenIdempotentInstructionAsync({
            payer: client.payer,
            owner: poorSender.address,
            mint: mint.address,
        });
        await client.sendTransaction([createATAIx]);

        await expect(
            client.pay.createTransfer({ recipient: recipient.address, amount: 1, splToken: mint.address }, poorSender),
        ).rejects.toThrow('insufficient funds');
    });

    it('should throw when mint account not found', async () => {
        const fakeMint = (await generateKeyPairSigner()).address;

        await expect(
            client.pay.createTransfer({ recipient: recipient.address, amount: 1, splToken: fakeMint }),
        ).rejects.toThrow('mint account not found');
    });
});
