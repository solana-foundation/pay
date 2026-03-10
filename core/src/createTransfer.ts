import {
    Account,
    AccountRole,
    type Address,
    type GetAccountInfoApi,
    type GetMultipleAccountsApi,
    type Instruction,
    type Rpc,
    type TransactionSigner,
} from '@solana/kit';
import { getAddMemoInstruction } from '@solana-program/memo';
import { getTransferSolInstruction, SYSTEM_PROGRAM_ADDRESS } from '@solana-program/system';
import {
    AccountState,
    fetchMint,
    fetchToken,
    findAssociatedTokenPda,
    getTransferCheckedInstruction,
    Mint,
    TOKEN_PROGRAM_ADDRESS,
} from '@solana-program/token';

import { SOL_DECIMALS, TOKEN_2022_PROGRAM_ADDRESS } from './constants.js';
import type { TransferFields } from './types.js';
import { amountToBaseUnits, decimalPlaces } from './utils/amount.js';
import { normalizeReferences } from './utils/reference.js';

/**
 * Thrown when a Solana Pay transfer transaction can't be created from the fields provided.
 */
export class CreateTransferError extends Error {
    name = 'CreateTransferError';
}

/**
 * Create instructions for a Solana Pay transfer.
 *
 * Returns an array of {@link Instruction} that the caller composes into a transaction message
 * using `pipe(createTransactionMessage(...), ...)`.
 *
 * @param rpc - An RPC client supporting `getAccountInfo`.
 * @param sender - The signer that will send the transfer.
 * @param fields - Fields of a Solana Pay transfer request URL.
 *
 * @throws {CreateTransferError}
 */
export async function createTransfer(
    rpc: Rpc<GetAccountInfoApi & GetMultipleAccountsApi>,
    sender: TransactionSigner,
    { recipient, amount, splToken, reference, memo }: TransferFields,
): Promise<Instruction[]> {
    const instructions: Instruction[] = [];

    // If a memo is provided, add it before the transfer instruction
    if (memo != null) {
        instructions.push(getAddMemoInstruction({ memo, signers: [sender] }));
    }

    // A native SOL or SPL token transfer instruction
    const transferInstruction = splToken
        ? await createSPLTokenInstruction(recipient, amount, splToken, sender, rpc)
        : await createSystemInstruction(recipient, amount, sender, rpc);

    // If reference accounts are provided, add them to the transfer instruction
    const refs = normalizeReferences(reference);
    if (refs) {
        const existingAccounts = transferInstruction.accounts ?? [];
        const refAccounts = refs.map(ref => ({
            address: ref,
            role: AccountRole.READONLY as const,
        }));
        const updatedInstruction = {
            ...transferInstruction,
            accounts: [...existingAccounts, ...refAccounts],
        };
        instructions.push(updatedInstruction);
    } else {
        instructions.push(transferInstruction);
    }

    return instructions;
}

async function createSystemInstruction(
    recipient: Address,
    amount: number,
    sender: TransactionSigner,
    rpc: Rpc<GetMultipleAccountsApi>,
): Promise<Instruction> {
    // Check that the sender and recipient accounts exist
    const {
        value: [senderInfo, recipientInfo],
    } = await rpc.getMultipleAccounts([sender.address, recipient], { encoding: 'base64' }).send();
    if (!senderInfo) throw new CreateTransferError('sender not found');
    if (!recipientInfo) throw new CreateTransferError('recipient not found');

    // Check that the sender and recipient are valid native accounts
    if (senderInfo.owner !== SYSTEM_PROGRAM_ADDRESS) throw new CreateTransferError('sender owner invalid');
    if (senderInfo.executable) throw new CreateTransferError('sender executable');
    if (recipientInfo.owner !== SYSTEM_PROGRAM_ADDRESS) throw new CreateTransferError('recipient owner invalid');
    if (recipientInfo.executable) throw new CreateTransferError('recipient executable');

    // Check that the amount provided doesn't have greater precision than SOL
    if (decimalPlaces(amount) > SOL_DECIMALS) throw new CreateTransferError('amount decimals invalid');

    // Convert input decimal amount to integer lamports
    const lamports = amountToBaseUnits(amount, SOL_DECIMALS);

    // Check that the sender has enough lamports
    if (lamports > senderInfo.lamports) throw new CreateTransferError('insufficient funds');

    // Create an instruction to transfer native SOL
    return getTransferSolInstruction({
        source: sender,
        destination: recipient,
        amount: lamports,
    });
}

async function createSPLTokenInstruction(
    recipient: Address,
    amount: number,
    splToken: Address,
    sender: TransactionSigner,
    rpc: Rpc<GetAccountInfoApi>,
): Promise<Instruction> {
    // Fetch the mint and determine the token program from its owner
    let mint: Account<Mint>;
    try {
        mint = await fetchMint(rpc, splToken);
    } catch {
        throw new CreateTransferError('mint account not found');
    }
    if (!mint.data.isInitialized) throw new CreateTransferError('mint not initialized');
    const tokenProgram: Address =
        mint.programAddress === TOKEN_2022_PROGRAM_ADDRESS ? TOKEN_2022_PROGRAM_ADDRESS : TOKEN_PROGRAM_ADDRESS;

    // Check that the amount provided doesn't have greater precision than the mint
    if (decimalPlaces(amount) > mint.data.decimals) throw new CreateTransferError('amount decimals invalid');

    // Convert input decimal amount to integer tokens according to the mint decimals
    const tokens = amountToBaseUnits(amount, mint.data.decimals);

    // Derive sender and recipient ATAs in parallel
    const [[senderATA], [recipientATA]] = await Promise.all([
        findAssociatedTokenPda({ owner: sender.address, tokenProgram, mint: splToken }),
        findAssociatedTokenPda({ owner: recipient, tokenProgram, mint: splToken }),
    ]);

    // Fetch both token accounts in parallel
    const [senderAccount, recipientAccount] = await Promise.all([
        fetchToken(rpc, senderATA),
        fetchToken(rpc, recipientATA),
    ]);

    if (senderAccount.data.state === AccountState.Uninitialized)
        throw new CreateTransferError('sender not initialized');
    if (senderAccount.data.state === AccountState.Frozen) throw new CreateTransferError('sender frozen');
    if (recipientAccount.data.state === AccountState.Uninitialized)
        throw new CreateTransferError('recipient not initialized');
    if (recipientAccount.data.state === AccountState.Frozen) throw new CreateTransferError('recipient frozen');

    // Check that the sender has enough tokens
    if (tokens > senderAccount.data.amount) throw new CreateTransferError('insufficient funds');

    // Create an instruction to transfer SPL tokens, asserting the mint and decimals match
    return getTransferCheckedInstruction(
        {
            source: senderATA,
            mint: splToken,
            destination: recipientATA,
            authority: sender,
            amount: tokens,
            decimals: mint.data.decimals,
        },
        { programAddress: tokenProgram },
    );
}
