import { describe, it, expect, beforeAll } from 'vitest';
import {
    createEmptyClient,
    createTransactionMessage,
    createTransactionPlanner,
    generateKeyPairSigner,
    lamports,
    pipe,
    setTransactionMessageFeePayerSigner,
    TransactionSigner,
} from '@solana/kit';
import {
    localhostRpc,
    planAndSendTransactions,
    generatedPayerWithSol,
    airdrop,
    defaultTransactionPlannerAndExecutorFromLitesvm,
} from '@solana/kit-plugins';
import { litesvm } from '@solana/kit-plugin-litesvm';
import {
    fetchToken,
    findAssociatedTokenPda,
    getCreateAssociatedTokenIdempotentInstructionAsync,
    TOKEN_PROGRAM_ADDRESS,
    getMintToATAInstructionPlanAsync,
    getCreateMintInstructionPlan,
} from '@solana-program/token';
import { solanaPay, encodeURL, parseURL } from '../src/index.js';

// --- Client Setup ---

async function createTestClient() {
    // Note: This is a test client for LiteSVM--b/c Solana Pay requires an RPC plugin and liteSVM only
    // exposes a subset of the RPC API, we include the localhost RPC plugin to make the test
    // client compatible with Solana Pay.
    // The tests do not test non-liteSVM methods (e.g., getSignaturesForAddress) which means we do not have
    // test cases for verify or fetchTransaction.
    const client = await createEmptyClient()
        .use(localhostRpc())
        .use(litesvm())
        .use(airdrop())
        .use(generatedPayerWithSol(lamports(100_000_000_000n)))
        .use(defaultTransactionPlannerAndExecutorFromLitesvm())
        // Override the transaction planner to remove the provisory SetComputeUnitLimit(0)
        // instruction added by defaultTransactionPlannerAndExecutorFromLitesvm(). The LiteSVM
        // executor never resolves this placeholder (unlike the RPC executor), causing all
        // transactions to fail with ComputationalBudgetExceeded.
        // This must come before planAndSendTransactions() since it captures the planner at call time.
        .use((client) => ({
            ...client,
            transactionPlanner: createTransactionPlanner({
                createTransactionMessage: () =>
                    pipe(createTransactionMessage({ version: 0 }), (tx) =>
                        setTransactionMessageFeePayerSigner(client.payer, tx)
                    ),
            }),
        }))
        .use(planAndSendTransactions())
        .use(solanaPay());

    return client;
}

type TestClient = Awaited<ReturnType<typeof createTestClient>>;

// --- SOL Transfers ---

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
            'insufficient funds'
        );
    });
});

// --- URL Round-trip ---

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

// --- SPL Token Transfers ---

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

        // Create mint
        await client.sendTransaction(
            getCreateMintInstructionPlan({ payer, newMint: mint, decimals: DECIMALS, mintAuthority: payer.address })
        );

        // Mint tokens to payer's ATA
        await client.sendTransaction(
            await getMintToATAInstructionPlanAsync({
                payer,
                mint: mint.address,
                mintAuthority: payer,
                amount: MINT_AMOUNT,
                decimals: DECIMALS,
                owner: payer.address,
            })
        );

        // Create recipient ATA (empty)
        await client.sendTransaction(
            await getMintToATAInstructionPlanAsync({
                payer,
                mint: mint.address,
                mintAuthority: payer,
                amount: 0n,
                decimals: DECIMALS,
                owner: recipient.address,
            })
        );
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
        const account = await fetchToken(client.rpc, recipientATA);
        expect(account.data.amount).toBe(MINT_AMOUNT);
    });

    it('should throw on insufficient SPL token balance', async () => {
        const poorSender = await generateKeyPairSigner();
        client.svm.airdrop(poorSender.address, lamports(1_000_000_000n));

        // Create ATA for poorSender with 0 tokens
        const createATAIx = await getCreateAssociatedTokenIdempotentInstructionAsync({
            payer: client.payer,
            owner: poorSender.address,
            mint: mint.address,
        });
        await client.sendTransaction([createATAIx]);

        await expect(
            client.pay.createTransfer({ recipient: recipient.address, amount: 1, splToken: mint.address }, poorSender)
        ).rejects.toThrow('insufficient funds');
    });

    it('should throw when mint account not found', async () => {
        const fakeMint = (await generateKeyPairSigner()).address;

        await expect(
            client.pay.createTransfer({ recipient: recipient.address, amount: 1, splToken: fakeMint })
        ).rejects.toThrow('mint account not found');
    });
});
