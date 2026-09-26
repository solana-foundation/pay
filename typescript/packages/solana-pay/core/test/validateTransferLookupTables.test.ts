import type { GetTransactionApi, Rpc, Signature } from '@solana/kit';
import {
    AccountRole,
    address,
    appendTransactionMessageInstructions,
    blockhash,
    compileTransaction,
    compressTransactionMessageUsingAddressLookupTables,
    createNoopSigner,
    createTransactionMessage,
    getBase64EncodedWireTransaction,
    getCompiledTransactionMessageDecoder,
    pipe,
    setTransactionMessageFeePayerSigner,
    setTransactionMessageLifetimeUsingBlockhash,
} from '@solana/kit';
import { getTransferSolInstruction } from '@solana-program/system';
import { findAssociatedTokenPda, getTransferCheckedInstruction, TOKEN_PROGRAM_ADDRESS } from '@solana-program/token';
import { describe, expect, it, vi } from 'vitest';

import { validateTransfer, ValidateTransferError } from '../src/index.js';

const SIGNATURE = '5UfDuX7hXbDBZpHnSEFMwBN6JdANTF54fGVz9Kp1fZBNTmRmEiGP' as Signature;
const SENDER = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
const RECIPIENT = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const MINT = address('Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB');
const REFERENCE = address('82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny');
const OTHER_REFERENCE = address('7dHbWXmci3dT1h5tC8S1ZLw6KcDk4chx6Y6bx4dM3f1h');
const TABLE_A = address('GfC73miMwXBoRYDn7gvEZVbhM7n6SUHxJb4LdBz2Mfp6');
const TABLE_B = address('4NpjLLnFBqFzwFkRFBauGFYVnijcQhLBSPo9UhbZgqRf');

async function createFixture(token: boolean, lookupRecipient = true) {
    const signer = createNoopSigner(SENDER);
    const [source] = await findAssociatedTokenPda({ owner: SENDER, mint: MINT, tokenProgram: TOKEN_PROGRAM_ADDRESS });
    const [destination] = await findAssociatedTokenPda({
        owner: RECIPIENT,
        mint: MINT,
        tokenProgram: TOKEN_PROGRAM_ADDRESS,
    });
    const recipientAccount = token ? destination : RECIPIENT;
    const transfer = token
        ? getTransferCheckedInstruction({
              source,
              destination,
              mint: MINT,
              authority: signer,
              amount: 1_000_000n,
              decimals: 6,
          })
        : getTransferSolInstruction({ source: signer, destination: RECIPIENT, amount: 1_000_000_000n });
    // Nonzero table indexes and two tables exercise the RPC's ordering:
    // all loaded writable addresses, then all loaded readonly addresses.
    const tables = {
        [TABLE_A]: [SENDER, REFERENCE, source],
        [TABLE_B]: [SENDER, OTHER_REFERENCE, MINT, ...(lookupRecipient ? [recipientAccount] : [])],
    };
    const message = pipe(
        createTransactionMessage({ version: 0 }),
        m => setTransactionMessageFeePayerSigner(signer, m),
        m =>
            setTransactionMessageLifetimeUsingBlockhash(
                { blockhash: blockhash(TABLE_B), lastValidBlockHeight: 100n },
                m,
            ),
        m =>
            appendTransactionMessageInstructions(
                [
                    {
                        ...transfer,
                        accounts: [
                            ...transfer.accounts,
                            { address: REFERENCE, role: AccountRole.READONLY },
                            { address: OTHER_REFERENCE, role: AccountRole.READONLY },
                        ],
                    },
                ],
                m,
            ),
        m => compressTransactionMessageUsingAddressLookupTables(m, tables),
    );
    const transaction = compileTransaction(message);
    const compiled = getCompiledTransactionMessageDecoder().decode(transaction.messageBytes);
    if (compiled.version !== 0) throw new Error('expected a v0 transaction');
    const lookups = compiled.addressTableLookups!;
    expect(lookups).toHaveLength(2);
    const loadedAddresses = {
        writable: lookups.flatMap(l => l.writableIndexes.map(i => tables[l.lookupTableAddress][i])),
        readonly: lookups.flatMap(l => l.readonlyIndexes.map(i => tables[l.lookupTableAddress][i])),
    };
    const accounts = [...compiled.staticAccounts, ...loadedAddresses.writable, ...loadedAddresses.readonly];
    const recipientIndex = accounts.indexOf(recipientAccount);
    const preBalances = accounts.map(() => 2_000_000_000n);
    const postBalances = [...preBalances];
    postBalances[0] -= 1_000_005_000n;
    if (!token) postBalances[recipientIndex] += 1_000_000_000n;
    const tokenBalance = {
        accountIndex: recipientIndex,
        mint: MINT,
        owner: RECIPIENT,
        programId: TOKEN_PROGRAM_ADDRESS,
    };
    const response = {
        meta: {
            err: null,
            preBalances,
            postBalances,
            loadedAddresses,
            preTokenBalances: [{ ...tokenBalance, uiTokenAmount: { amount: '2000000', decimals: 6 } }],
            postTokenBalances: [{ ...tokenBalance, uiTokenAmount: { amount: '3000000', decimals: 6 } }],
        },
        transaction: [getBase64EncodedWireTransaction(transaction), 'base64'],
    };
    const send = vi.fn().mockResolvedValue(response);
    const rpc = { getTransaction: vi.fn().mockReturnValue({ send }) } as unknown as Rpc<GetTransactionApi>;
    const fields = {
        recipient: RECIPIENT,
        amount: 1,
        reference: [REFERENCE, OTHER_REFERENCE],
        ...(token ? { splToken: MINT } : {}),
    };
    return { rpc, response, fields, recipientIndex };
}

describe('validateTransfer with address lookup tables', () => {
    it.each([false, true])('validates a transfer with a loaded recipient (SPL token: %s)', async token => {
        const { rpc, response, fields } = await createFixture(token);
        await expect(validateTransfer(rpc, SIGNATURE, fields)).resolves.toBe(response);
    });

    it.each([false, true])('validates a static recipient with loaded references (SPL token: %s)', async token => {
        const { rpc, response, fields } = await createFixture(token, false);
        await expect(validateTransfer(rpc, SIGNATURE, fields)).resolves.toBe(response);
    });

    it.each([false, true])('rejects insufficient transferred amounts (SPL token: %s)', async token => {
        const { rpc, fields } = await createFixture(token);
        await expect(validateTransfer(rpc, SIGNATURE, { ...fields, amount: 2 })).rejects.toThrow(
            'amount not transferred',
        );
    });

    it.each([false, true])('rejects a different recipient (SPL token: %s)', async token => {
        const { rpc, fields } = await createFixture(token);
        await expect(validateTransfer(rpc, SIGNATURE, { ...fields, recipient: SENDER })).rejects.toThrow(
            'invalid transfer',
        );
    });

    it('rejects a different mint', async () => {
        const { rpc, fields } = await createFixture(true);
        await expect(validateTransfer(rpc, SIGNATURE, { ...fields, splToken: SENDER })).rejects.toThrow(
            'invalid transfer',
        );
    });

    it('rejects a different reference', async () => {
        const { rpc, fields } = await createFixture(false);
        await expect(
            validateTransfer(rpc, SIGNATURE, { ...fields, reference: [OTHER_REFERENCE, REFERENCE] }),
        ).rejects.toThrow('invalid reference 0');
    });

    it('rejects missing loaded addresses', async () => {
        const { rpc, response, fields } = await createFixture(false);
        Reflect.deleteProperty(response.meta, 'loadedAddresses');
        await expect(validateTransfer(rpc, SIGNATURE, fields)).rejects.toThrow(ValidateTransferError);
    });

    it.each(['writable', 'readonly'] as const)('rejects truncated loaded %s addresses', async kind => {
        const { rpc, response, fields } = await createFixture(true);
        response.meta.loadedAddresses[kind].pop();
        await expect(validateTransfer(rpc, SIGNATURE, fields)).rejects.toThrow(ValidateTransferError);
    });

    it.each(['writable', 'readonly'] as const)('rejects unexpected extra loaded %s addresses', async kind => {
        const { rpc, response, fields } = await createFixture(true);
        response.meta.loadedAddresses[kind].push(SENDER);
        await expect(validateTransfer(rpc, SIGNATURE, fields)).rejects.toThrow(ValidateTransferError);
    });
});
