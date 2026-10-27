const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { createRequire } = require('node:module');
const { test } = require('node:test');
const React = require('react');
const { act, create } = require('react-test-renderer');
const ts = require('typescript');
const kit = require('@solana/kit');
const pay = require('@solana/pay');
const { isFailedTransaction, litesvm } = require('@solana/kit-plugin-litesvm');
const { airdropPayer, generatedPayer } = require('@solana/kit-plugin-signer');
const {
    findAssociatedTokenPda,
    getCreateAssociatedTokenIdempotentInstructionAsync,
    getTokenDecoder,
    TOKEN_PROGRAM_ADDRESS,
    tokenProgram,
} = require('@solana-program/token');

const providerPath = path.resolve(__dirname, '../src/client/components/contexts/PaymentProvider.tsx');
const realSetImmediate = setImmediate;

// Mount the real provider without starting Next or a wallet extension. Only
// framework boundaries are replaced; the provider and its relative modules
// are compiled directly from source with the example's TypeScript dependency.
function sourceLoader(mocks) {
    const cache = new Map();
    const load = filename => {
        if (cache.has(filename)) return cache.get(filename).exports;
        const module = { exports: {} };
        cache.set(filename, module);
        const code = ts.transpileModule(fs.readFileSync(filename, 'utf8'), {
            compilerOptions: {
                target: ts.ScriptTarget.ES2022,
                module: ts.ModuleKind.CommonJS,
                jsx: ts.JsxEmit.React,
                esModuleInterop: true,
            },
        }).outputText;
        const localRequire = createRequire(filename);
        const requireSource = id => {
            if (Object.hasOwn(mocks, id)) return mocks[id];
            if (id.startsWith('.')) {
                const base = path.resolve(path.dirname(filename), id.replace(/\.js$/, ''));
                for (const extension of ['.ts', '.tsx']) {
                    if (fs.existsSync(base + extension)) return load(base + extension);
                }
            }
            return localRequire(id);
        };
        new Function('require', 'module', 'exports', code)(requireSource, module, module.exports);
        return module.exports;
    };
    return load;
}

async function fixture(
    t,
    {
        outcome = 'success',
        transactionRequest = false,
        holdWallet = false,
        holdSend = false,
        connectWallet = true,
    } = {},
) {
    const client = await kit
        .createClient()
        .use(await generatedPayer())
        .use(litesvm())
        .use(airdropPayer(kit.lamports(100_000_000_000n)))
        .use(tokenProgram());
    const payer = client.payer;
    const merchant = await kit.generateKeyPairSigner();
    const mint = await kit.generateKeyPairSigner();
    await client.token.instructions
        .createMint({ payer, newMint: mint, decimals: 6, mintAuthority: payer.address })
        .sendTransaction();
    await client.token.instructions
        .mintToATA({
            payer,
            mint: mint.address,
            amount: 10_000_000n,
            decimals: 6,
            mintAuthority: payer,
            owner: payer.address,
        })
        .sendTransaction();
    const [source] = await findAssociatedTokenPda({
        owner: payer.address,
        mint: mint.address,
        tokenProgram: TOKEN_PROGRAM_ADDRESS,
    });
    const [destination] = await findAssociatedTokenPda({
        owner: merchant.address,
        mint: mint.address,
        tokenProgram: TOKEN_PROGRAM_ADDRESS,
    });
    await client.sendTransaction([
        await getCreateAssociatedTokenIdempotentInstructionAsync({
            payer,
            owner: merchant.address,
            mint: mint.address,
        }),
    ]);
    const balances = () =>
        [source, destination].map(a => getTokenDecoder().decode(client.svm.getAccount(a).data).amount);
    assert.deepEqual(balances(), [10_000_000n, 0n]);

    const counts = { signs: 0, sends: 0, transfers: 0 };
    let referenceVisible = false;
    let failPreparation = false;
    let walletConnected = true;
    let latest;
    let renderer;
    let releaseWallet;
    const walletResult = new Promise(resolve => {
        releaseWallet = resolve;
    });
    let releaseSend;
    const sendResult = new Promise(resolve => {
        releaseSend = resolve;
    });
    const wires = [];
    const signer = {
        ...payer,
        async modifyAndSignTransactions(transactions) {
            counts.signs++;
            if (outcome === 'decline') throw new Error('User rejected the wallet request');
            const maps = await payer.signTransactions(transactions);
            const signed = transactions.map((tx, i) => ({ ...tx, signatures: { ...tx.signatures, ...maps[i] } }));
            if (holdWallet) await walletResult;
            return signed;
        },
    };
    let currentSigner = signer;
    const rpc = {
        getAccountInfo: (...args) => client.rpc.getAccountInfo(...args),
        getMultipleAccounts: (...args) => client.rpc.getMultipleAccounts(...args),
        getLatestBlockhash: () =>
            outcome === 'preparation-failed' || failPreparation
                ? {
                      send: async () => {
                          throw new Error('Failed while preparing the transaction');
                      },
                  }
                : client.rpc.getLatestBlockhash(),
        getSignatureStatuses: () => ({ send: async () => ({ value: [{ err: null, confirmations: 1 }] }) }),
        sendTransaction(wire) {
            return {
                async send() {
                    counts.sends++;
                    wires.push(wire);
                    if (outcome === 'before-submit') throw new Error('Connection lost before submission');
                    const tx = kit.getTransactionDecoder().decode(kit.getBase64Encoder().encode(wire));
                    const result = client.svm.sendTransaction(tx);
                    assert.equal(isFailedTransaction(result), false, result.toString());
                    counts.transfers++;
                    if (holdSend) await sendResult;
                    if (outcome === 'lost-response') throw new Error('Response lost after accepted submission');
                    return kit.getSignatureFromTransaction(tx);
                },
            };
        },
    };
    const config = {
        recipient: merchant.address,
        splToken: mint.address,
        label: 'Local test merchant',
        requiredConfirmations: 1,
        connectWallet,
        link: transactionRequest ? 'https://merchant.invalid/api' : undefined,
    };
    const navigate = () => {};
    const load = sourceLoader({
        '@solana/pay': {
            ...pay,
            async fetchTransaction(_rpc, _address, link) {
                const instructions = await pay.createTransfer(rpc, signer, {
                    recipient: merchant.address,
                    amount: 1,
                    splToken: mint.address,
                    reference: kit.address(link.searchParams.get('reference')),
                });
                return kit.compileTransaction(
                    kit.appendTransactionMessageInstructions(
                        instructions,
                        kit.setTransactionMessageLifetimeUsingBlockhash(
                            (await rpc.getLatestBlockhash().send()).value,
                            kit.setTransactionMessageFeePayer(
                                payer.address,
                                kit.createTransactionMessage({ version: 0 }),
                            ),
                        ),
                    ),
                );
            },
            async findReference() {
                if (!referenceVisible) throw new pay.FindReferenceError('not found');
                return {
                    signature: kit.getSignatureFromTransaction(
                        kit.getTransactionDecoder().decode(kit.getBase64Encoder().encode(wires[0])),
                    ),
                };
            },
            validateTransfer: async () => {},
        },
        '@solana/kit': { ...kit, createSolanaRpc: () => rpc },
        '@solana/connector/react': {
            useAccount: () => ({ address: walletConnected ? payer.address : undefined, connected: walletConnected }),
            useConnectWallet: () => ({ connect() {} }),
            useWalletConnectors: () => [],
            useKitTransactionSigner: () => ({ signer: currentSigner }),
        },
        'next/router': { useRouter: () => ({ query: { amount: '1' } }) },
        './PendingPage.module.css': {},
        '../buttons/BackButton': {
            BackButton: ({ children, onClick }) => React.createElement('button', { onClick }, children),
        },
        '../sections/Amount': { Amount: ({ amount }) => React.createElement('span', null, String(amount)) },
        '../sections/PoweredBy': { PoweredBy: () => null },
        '../sections/QRCode': { QRCode: () => React.createElement('span', null, 'fixture QR code') },
        '../../hooks/useConfig': { useConfig: () => config },
        '../../hooks/useNavigateWithQuery': { useNavigateWithQuery: () => navigate },
    });
    const { PaymentProvider } = load(providerPath);
    const { usePayment } = load(path.resolve(path.dirname(providerPath), '../../hooks/usePayment.ts'));
    const { default: PendingPage } = load(path.resolve(path.dirname(providerPath), '../pages/PendingPage.tsx'));
    function Observer() {
        latest = usePayment();
        return React.createElement(PendingPage);
    }
    const element = () => React.createElement(PaymentProvider, null, React.createElement(Observer));
    const timeouts = new Map();
    const intervals = new Map();
    const pending = [];
    let clock = 0;
    let timerId = 0;
    const original = { setTimeout, clearTimeout, setInterval, clearInterval, error: console.error };
    global.setTimeout = (fn, delay = 0) => {
        const id = ++timerId;
        timeouts.set(id, { fn, at: clock + delay });
        return id;
    };
    global.clearTimeout = id => timeouts.delete(id);
    global.setInterval = fn => {
        const id = ++timerId;
        intervals.set(id, fn);
        return id;
    };
    global.clearInterval = id => intervals.delete(id);
    console.error = () => {};
    t.after(async () => {
        releaseWallet();
        releaseSend();
        await act(async () => renderer?.unmount());
        Object.assign(global, {
            setTimeout: original.setTimeout,
            clearTimeout: original.clearTimeout,
            setInterval: original.setInterval,
            clearInterval: original.clearInterval,
        });
        console.error = original.error;
    });
    const flush = async () => {
        for (let i = 0; i < 4; i++) await new Promise(resolve => realSetImmediate(resolve));
    };
    const advance = async (ms, wait = true) => {
        const end = clock + ms;
        for (;;) {
            const next = [...timeouts].filter(([, v]) => v.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
            if (!next) break;
            timeouts.delete(next[0]);
            clock = next[1].at;
            await act(async () => {
                const result = next[1].fn();
                pending.push(result);
                if (wait) await result;
                await flush();
            });
        }
        clock = end;
    };
    await act(async () => {
        renderer = create(element());
    });
    await act(async () => {
        await latest.generate();
    });
    await advance(0, !holdWallet && !holdSend);
    if (holdWallet) {
        for (let i = 0; i < 100 && !counts.signs; i++) await act(flush);
        assert.equal(counts.signs, 1);
    }
    if (holdSend) {
        for (let i = 0; i < 100 && !counts.sends; i++) await act(flush);
        assert.equal(counts.sends, 1);
    }
    return {
        counts,
        balances,
        wires,
        client,
        advance,
        payment: () => latest,
        rendered: () => JSON.stringify(renderer.toJSON()),
        async restartEffect(fail = false) {
            failPreparation = fail;
            currentSigner = { ...signer };
            await act(async () => renderer.update(element()));
            await advance(0, !holdWallet && !holdSend);
        },
        async disconnectWallet() {
            walletConnected = false;
            currentSigner = undefined;
            await act(async () => renderer.update(element()));
            await advance(0);
        },
        async discover() {
            referenceVisible = true;
            await act(async () => {
                for (const fn of [...intervals.values()]) await fn();
            });
            await advance(0);
        },
        async unmount() {
            await act(async () => {
                renderer.unmount();
                renderer = undefined;
            });
        },
        async release() {
            releaseWallet();
            await act(async () => {
                await Promise.all(pending);
                await flush();
            });
        },
        async finishSend() {
            releaseSend();
            await act(async () => {
                await Promise.all(pending);
                await flush();
            });
        },
        async newInvoice() {
            await act(async () => latest.reset());
            await act(async () => latest.setAmount(1));
            await act(async () => latest.generate());
            await advance(0);
        },
    };
}

for (const transactionRequest of [false, true]) {
    const kind = transactionRequest ? 'transaction request' : 'transfer request';
    test(`${kind}: an accepted payment with a lost response transfers only once`, async t => {
        const f = await fixture(t, { outcome: 'lost-response', transactionRequest });
        assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
        f.client.svm.expireBlockhash(); // A later latestBlockhash would produce a different transaction.
        await f.advance(10_000);
        await f.restartEffect();
        t.diagnostic(
            `Synthetic payer/merchant balances: 10/0 before; 9/1 after first send; ${f
                .balances()
                .map(raw => Number(raw) / 1_000_000)
                .join('/')} after response loss and effect restart`,
        );
        assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
        assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
        assert.match(f.payment().submissionError, /result is unknown/);
        assert.match(f.payment().submissionError, /Check your wallet/);
        assert.doesNotMatch(f.rendered(), /fixture QR code|Approve this payment/);
        await f.discover();
        assert.equal(f.payment().status, 'Valid');
        assert.equal(
            f.payment().signature,
            kit.getSignatureFromTransaction(
                kit.getTransactionDecoder().decode(kit.getBase64Encoder().encode(f.wires[0])),
            ),
        );
    });
}

test('a successful payment is not repeated when the signer object changes', async t => {
    const f = await fixture(t);
    await f.restartEffect();
    await f.advance(10_000);
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
    assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
    assert.equal(f.payment().submissionError, undefined);
});

for (const outcome of ['before-submit', 'decline']) {
    test(`${outcome}: failure does not automatically request another wallet signature`, async t => {
        const f = await fixture(t, { outcome });
        await f.advance(10_000);
        await f.restartEffect();
        assert.equal(f.counts.signs, 1);
        assert.equal(f.counts.transfers, 0);
        assert.deepEqual(f.balances(), [10_000_000n, 0n]);
        assert.match(
            f.payment().submissionError,
            outcome === 'decline' ? /did not reach transaction submission/ : /result is unknown/,
        );
        assert.doesNotMatch(f.rendered(), /fixture QR code|Approve this payment/);
    });
}

test('unmounting while the wallet prompt is open prevents a later broadcast', async t => {
    const f = await fixture(t, { holdWallet: true });
    await f.unmount();
    await f.release();
    assert.deepEqual(f.counts, { signs: 1, sends: 0, transfers: 0 });
    assert.deepEqual(f.balances(), [10_000_000n, 0n]);
});

test('an effect restart shares an in-flight wallet prompt and broadcasts once', async t => {
    const f = await fixture(t, { holdWallet: true });
    await f.restartEffect();
    await f.release();
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
    assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
});

test('an effect restart observes the original in-flight send failure', async t => {
    const f = await fixture(t, { outcome: 'lost-response', holdSend: true });
    assert.equal(f.payment().submissionError, undefined);
    await f.restartEffect();
    await f.finishSend();
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
    assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
    assert.match(f.payment().submissionError, /Check your wallet/);
});

test('an explicitly generated new invoice can authorize its own payment', async t => {
    const f = await fixture(t);
    const firstReference = f.payment().reference;
    f.client.svm.expireBlockhash();
    await f.newInvoice();
    assert.notEqual(f.payment().reference, firstReference);
    assert.deepEqual(f.counts, { signs: 2, sends: 2, transfers: 2 });
    assert.deepEqual(f.balances(), [8_000_000n, 2_000_000n]);
});

test('the default QR flow does not sign or submit through the connected wallet', async t => {
    const f = await fixture(t, { connectWallet: false });
    assert.deepEqual(f.counts, { signs: 0, sends: 0, transfers: 0 });
    assert.match(f.rendered(), /fixture QR code/);
    assert.match(f.rendered(), /Scan this code/);
    assert.doesNotMatch(f.rendered(), /Approve this payment/);
});

test('a preparation failure reports that this attempt never reached submission', async t => {
    const f = await fixture(t, { outcome: 'preparation-failed' });
    assert.deepEqual(f.counts, { signs: 0, sends: 0, transfers: 0 });
    assert.match(f.payment().submissionError, /did not reach transaction submission/);
    assert.doesNotMatch(f.rendered(), /fixture QR code|Approve this payment/);
});

test('an effect restart awaits the original submission even if fresh preparation would fail', async t => {
    const f = await fixture(t, { outcome: 'lost-response', holdSend: true });
    assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
    await f.restartEffect(true);
    assert.equal(f.payment().submissionError, undefined);
    await f.finishSend();
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
    assert.match(f.payment().submissionError, /result is unknown/);
    assert.doesNotMatch(f.rendered(), /did not reach|fixture QR code/);
});

test('disconnecting after a payment does not offer another QR payment', async t => {
    const f = await fixture(t);
    await f.disconnectWallet();
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
    assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
    assert.doesNotMatch(f.rendered(), /fixture QR code|Scan this code/);
    assert.match(f.rendered(), /already approved this payment/);
});

for (const transactionRequest of [false, true]) {
    const kind = transactionRequest ? 'transaction request' : 'transfer request';
    test(`${kind}: disconnecting during a send preserves its later unknown outcome`, async t => {
        const f = await fixture(t, { outcome: 'lost-response', holdSend: true, transactionRequest });
        await f.disconnectWallet();
        await f.finishSend();
        assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
        assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
        assert.match(f.payment().submissionError, /result is unknown/);
        assert.match(f.rendered(), /Check your wallet/);
        assert.doesNotMatch(f.rendered(), /fixture QR code|Approve this payment|Connect your wallet to continue/);
        await f.discover();
        assert.equal(f.payment().status, 'Valid');
    });
}

test('reference discovery still confirms a successful send completed after disconnect', async t => {
    const f = await fixture(t, { holdSend: true });
    await f.disconnectWallet();
    await f.finishSend();
    assert.equal(f.payment().submissionError, undefined);
    await f.discover();
    assert.equal(f.payment().status, 'Valid');
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
});

test('reset ignores a previous invoice send failure', async t => {
    const f = await fixture(t, { outcome: 'lost-response', holdSend: true });
    await act(async () => f.payment().reset());
    await f.finishSend();
    assert.equal(f.payment().status, 'New');
    assert.equal(f.payment().reference, undefined);
    assert.equal(f.payment().submissionError, undefined);
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
});

test('a confirmed invoice ignores a later send failure', async t => {
    const f = await fixture(t, { outcome: 'lost-response', holdSend: true });
    await f.discover();
    assert.equal(f.payment().status, 'Valid');
    await f.finishSend();
    assert.equal(f.payment().submissionError, undefined);
    assert.equal(f.payment().status, 'Valid');
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
});

test('unmounting during a send does not resubmit when its response fails', async t => {
    const f = await fixture(t, { outcome: 'lost-response', holdSend: true });
    await f.unmount();
    await f.finishSend();
    assert.deepEqual(f.counts, { signs: 1, sends: 1, transfers: 1 });
    assert.deepEqual(f.balances(), [9_000_000n, 1_000_000n]);
});
