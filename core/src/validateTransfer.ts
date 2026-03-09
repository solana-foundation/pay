import type { Address, GetTransactionApi, Lamports, Rpc, Signature, TokenBalance, TransactionError } from '@solana/kit';
import { getBase58Encoder } from '@solana/kit';
import { SYSTEM_PROGRAM_ADDRESS } from '@solana-program/system';
import { findAssociatedTokenPda, TOKEN_PROGRAM_ADDRESS } from '@solana-program/token';

import { MEMO_PROGRAM_ADDRESS, SOL_DECIMALS, TOKEN_2022_PROGRAM_ADDRESS } from './constants.js';
import type { Amount, Finality, Memo, Recipient, Reference, References, SPLToken } from './types.js';
import { amountToBaseUnits } from './utils/amount.js';

const base58Encoder = getBase58Encoder();

/**
 * Thrown when a transaction doesn't contain a valid Solana Pay transfer.
 */
export class ValidateTransferError extends Error {
    name = 'ValidateTransferError';
}

/**
 * Fields of a Solana Pay transfer request to validate.
 */
export interface ValidateTransferFields {
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
 * A JSON-encoded transaction instruction from the RPC getTransaction response.
 */
interface RpcTransactionInstruction {
    accounts: readonly number[];
    data: string;
    programIdIndex: number;
}

/**
 * The shape of a getTransaction JSON response (non-parsed).
 */
export interface GetTransactionJsonResponse {
    meta: {
        err: TransactionError | null;
        preBalances: readonly Lamports[];
        postBalances: readonly Lamports[];
        preTokenBalances?: readonly TokenBalance[];
        postTokenBalances?: readonly TokenBalance[];
    } | null;
    transaction: {
        message: {
            accountKeys: readonly Address[];
            instructions: readonly RpcTransactionInstruction[];
        };
        signatures: readonly string[];
    };
}

/** Balance change result from validation helpers. */
interface BalanceChange {
    pre: bigint;
    post: bigint;
    decimals: number;
}

/**
 * Check that a given transaction contains a valid Solana Pay transfer.
 *
 * @param rpc - An RPC client supporting `getTransaction`.
 * @param signature - The signature of the transaction to validate.
 * @param fields - Fields of a Solana Pay transfer request to validate.
 * @param options - Options for `getTransaction`.
 *
 * @throws {ValidateTransferError}
 */
export async function validateTransfer(
    rpc: Rpc<GetTransactionApi>,
    signature: Signature,
    { recipient, amount, splToken, reference, memo }: ValidateTransferFields,
    options?: { commitment?: Finality },
): Promise<GetTransactionJsonResponse> {
    if (!Number.isFinite(amount) || amount < 0) {
        throw new ValidateTransferError('amount invalid');
    }

    const response = (await rpc
        .getTransaction(signature, {
            commitment: options?.commitment,
            maxSupportedTransactionVersion: 0,
            encoding: 'json',
        })
        .send()) as GetTransactionJsonResponse | null;

    if (!response) throw new ValidateTransferError('not found');

    const { meta } = response;
    if (!meta) throw new ValidateTransferError('missing meta');
    if (meta.err) throw new ValidateTransferError(JSON.stringify(meta.err));

    const { accountKeys, instructions: allInstructions } = response.transaction.message;

    // Normalize reference to array
    let refs: Reference[] | undefined;
    if (reference) {
        refs = Array.isArray(reference) ? reference : [reference];
    }

    // Make a copy of the instructions we're going to validate
    const instructions = [...allInstructions];

    // Transfer instruction must be the last instruction
    const instruction = instructions.pop();
    if (!instruction) throw new ValidateTransferError('missing transfer instruction');

    const { pre, post, decimals } = splToken
        ? await validateSPLTokenTransfer(instruction, accountKeys, meta, recipient, splToken, refs)
        : validateSystemTransfer(instruction, accountKeys, meta, recipient, refs);

    const expected = amountToBaseUnits(amount, decimals);
    if (post - pre < expected) throw new ValidateTransferError('amount not transferred');

    if (memo !== undefined) {
        // Memo instruction must be the second to last instruction
        const memoInstruction = instructions.pop();
        if (!memoInstruction) throw new ValidateTransferError('missing memo instruction');
        validateMemo(memoInstruction, accountKeys, memo);
    }

    return response;
}

function base58ToBytes(data: string) {
    try {
        return base58Encoder.encode(data);
    } catch {
        throw new ValidateTransferError('invalid instruction data');
    }
}

function validateMemo(instruction: RpcTransactionInstruction, accountKeys: readonly Address[], memo: string): void {
    const programId = accountKeys[instruction.programIdIndex];
    if (programId !== MEMO_PROGRAM_ADDRESS) throw new ValidateTransferError('invalid memo program');

    // Decode the base58 instruction data and compare with expected memo
    const encoder = new TextEncoder();
    const expectedBytes = encoder.encode(memo);
    const decodedData = base58ToBytes(instruction.data);
    if (decodedData.length !== expectedBytes.length) throw new ValidateTransferError('invalid memo');
    for (let i = 0; i < decodedData.length; i++) {
        if (decodedData[i] !== expectedBytes[i]) throw new ValidateTransferError('invalid memo');
    }
}

function validateSystemTransfer(
    instruction: RpcTransactionInstruction,
    accountKeys: readonly Address[],
    meta: NonNullable<GetTransactionJsonResponse['meta']>,
    recipient: Address,
    references?: Reference[],
): BalanceChange {
    const accountIndex = accountKeys.indexOf(recipient);
    if (accountIndex === -1) throw new ValidateTransferError('recipient not found');

    if (references) {
        if (accountKeys[instruction.programIdIndex] !== SYSTEM_PROGRAM_ADDRESS) {
            throw new ValidateTransferError('invalid transfer');
        }

        // Check that the expected reference keys exactly match the extra keys provided to the instruction
        const extraKeys = instruction.accounts.slice(2);
        const length = extraKeys.length;
        if (length !== references.length) throw new ValidateTransferError('invalid references');

        for (let i = 0; i < length; i++) {
            if (accountKeys[extraKeys[i]] !== references[i]) throw new ValidateTransferError(`invalid reference ${i}`);
        }
    }

    const pre = meta.preBalances[accountIndex];
    const post = meta.postBalances[accountIndex];
    if (pre === undefined || post === undefined) throw new ValidateTransferError('missing balance data');
    return { pre, post, decimals: SOL_DECIMALS };
}

async function validateSPLTokenTransfer(
    instruction: RpcTransactionInstruction,
    accountKeys: readonly Address[],
    meta: NonNullable<GetTransactionJsonResponse['meta']>,
    recipient: Address,
    splToken: Address,
    references?: Reference[],
): Promise<BalanceChange> {
    // Use programId from the instruction itself to derive correct ATA
    const programId = accountKeys[instruction.programIdIndex];
    const tokenProgram = programId === TOKEN_2022_PROGRAM_ADDRESS ? TOKEN_2022_PROGRAM_ADDRESS : TOKEN_PROGRAM_ADDRESS;
    const [recipientATA] = await findAssociatedTokenPda({
        owner: recipient,
        tokenProgram,
        mint: splToken,
    });
    const accountIndex = accountKeys.indexOf(recipientATA);
    if (accountIndex === -1) throw new ValidateTransferError('recipient not found');

    if (references) {
        // Check that the instruction is an SPL token transfer or transferChecked instruction
        if (programId !== TOKEN_PROGRAM_ADDRESS && programId !== TOKEN_2022_PROGRAM_ADDRESS) {
            throw new ValidateTransferError('invalid transfer');
        }

        // For transferChecked: accounts = [source, mint, destination, authority, ...multiSigners]
        // For transfer: accounts = [source, destination, authority, ...multiSigners]
        const decoded = base58ToBytes(instruction.data);
        const transferDataByte = decoded[0];
        if (transferDataByte !== 3 && transferDataByte !== 12) {
            throw new ValidateTransferError('invalid transfer instruction');
        }
        // 3 = Transfer, 12 = TransferChecked
        const requiredAccounts = transferDataByte === 12 ? 4 : 3;
        const extraKeys = instruction.accounts.slice(requiredAccounts);
        const length = extraKeys.length;
        if (length !== references.length) throw new ValidateTransferError('invalid references');

        for (let i = 0; i < length; i++) {
            if (accountKeys[extraKeys[i]] !== references[i]) throw new ValidateTransferError(`invalid reference ${i}`);
        }
    }

    const preBalance = meta.preTokenBalances?.find(x => x.accountIndex === accountIndex);
    const postBalance = meta.postTokenBalances?.find(x => x.accountIndex === accountIndex);
    if (!preBalance || !postBalance) throw new ValidateTransferError('missing balance data');

    return {
        pre: BigInt(preBalance.uiTokenAmount.amount),
        post: BigInt(postBalance.uiTokenAmount.amount),
        decimals: preBalance.uiTokenAmount.decimals,
    };
}
