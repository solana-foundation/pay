import {
    AccountRole,
    type Address,
    type GetAccountInfoApi,
    type Instruction,
    type Rpc,
    type TransactionSigner,
} from '@solana/kit';
import { getTransferSolInstruction, SYSTEM_PROGRAM_ADDRESS } from '@solana-program/system';
import {
    fetchMint,
    fetchToken,
    findAssociatedTokenPda,
    getTransferCheckedInstruction,
    TOKEN_PROGRAM_ADDRESS,
} from '@solana-program/token';
import { getAddMemoInstruction } from '@solana-program/memo';
import { SOL_DECIMALS, TOKEN_2022_PROGRAM_ADDRESS } from './constants.js';
import { amountToBaseUnits, decimalPlaces } from './utils/amount.js';
import type { Amount, Memo, Recipient, References, SPLToken } from './types.js';

/**
 * Thrown when a Solana Pay transfer transaction can't be created from the fields provided.
 */
export class CreateTransferError extends Error {
    name = 'CreateTransferError';
}

/**
 * Fields of a Solana Pay transfer request URL.
 */
export interface CreateTransferFields {
    /** `recipient` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#recipient). */
    recipient: Recipient;
    /** `amount` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#amount). */
    amount: Amount;
    /** `spl-token` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#spl-token). */
    splToken?: SPLToken;
    /** `reference` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#reference). */
    reference?: References;
    /** `memo` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#memo). */
    memo?: Memo;
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
    rpc: Rpc<GetAccountInfoApi>,
    sender: TransactionSigner,
    { recipient, amount, splToken, reference, memo }: CreateTransferFields
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
    if (reference) {
        const refs = Array.isArray(reference) ? reference : [reference];
        const existingAccounts = transferInstruction.accounts ?? [];
        const refAccounts = refs.map((ref) => ({
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
    rpc: Rpc<GetAccountInfoApi>
): Promise<Instruction> {
    // Check that the sender and recipient accounts exist
    const senderInfo = (await rpc.getAccountInfo(sender.address, { encoding: 'base64' }).send()).value;
    if (!senderInfo) throw new CreateTransferError('sender not found');

    const recipientInfo = (await rpc.getAccountInfo(recipient, { encoding: 'base64' }).send()).value;
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
    rpc: Rpc<GetAccountInfoApi>
): Promise<Instruction> {
    // Check if token is owned by Token-2022 Program
    const accountInfo = (await rpc.getAccountInfo(splToken, { encoding: 'base64' }).send()).value;
    if (!accountInfo) throw new CreateTransferError('mint account not found');
    const tokenProgram: Address =
        accountInfo.owner === TOKEN_2022_PROGRAM_ADDRESS ? TOKEN_2022_PROGRAM_ADDRESS : TOKEN_PROGRAM_ADDRESS;

    // Check that the token provided is an initialized mint
    const mint = await fetchMint(rpc, splToken);
    if (!mint.data.isInitialized) throw new CreateTransferError('mint not initialized');

    // Check that the amount provided doesn't have greater precision than the mint
    if (decimalPlaces(amount) > mint.data.decimals) throw new CreateTransferError('amount decimals invalid');

    // Convert input decimal amount to integer tokens according to the mint decimals
    const tokens = amountToBaseUnits(amount, mint.data.decimals);

    // Get the sender's ATA and check that the account exists and can send tokens
    const [senderATA] = await findAssociatedTokenPda({
        owner: sender.address,
        tokenProgram,
        mint: splToken,
    });
    const senderAccount = await fetchToken(rpc, senderATA);
    if (senderAccount.data.state === 0) throw new CreateTransferError('sender not initialized');
    if (senderAccount.data.state === 2) throw new CreateTransferError('sender frozen');

    // Get the recipient's ATA and check that the account exists and can receive tokens
    const [recipientATA] = await findAssociatedTokenPda({
        owner: recipient,
        tokenProgram,
        mint: splToken,
    });
    const recipientAccount = await fetchToken(rpc, recipientATA);
    if (recipientAccount.data.state === 0) throw new CreateTransferError('recipient not initialized');
    if (recipientAccount.data.state === 2) throw new CreateTransferError('recipient frozen');

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
        { programAddress: tokenProgram }
    );
}
