import { SOLANA_PROTOCOL } from './constants.js';
import type { Amount, Label, Memo, Message, Recipient, References, SPLToken } from './types.js';
import { normalizeReferences } from './utils/reference.js';

/**
 * Fields of a Solana Pay transaction request URL.
 */
export interface TransactionRequestURLFields {
    /** `link` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#link). */
    link: URL;
    /** `label` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#label-1). */
    label?: Label;
    /** `message` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#message-1).  */
    message?: Message;
}

/**
 * Fields of a Solana Pay transfer request URL.
 */
export interface TransferRequestURLFields {
    /** `recipient` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#recipient). */
    recipient: Recipient;
    /** `amount` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#amount). */
    amount?: Amount;
    /** `spl-token` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#spl-token). */
    splToken?: SPLToken;
    /** `reference` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#reference). */
    reference?: References;
    /** `label` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#label). */
    label?: Label;
    /** `message` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#message).  */
    message?: Message;
    /** `memo` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#memo). */
    memo?: Memo;
}

/**
 * Encode a Solana Pay URL.
 *
 * @param fields Fields to encode in the URL.
 */
export function encodeURL(fields: TransactionRequestURLFields | TransferRequestURLFields): URL {
    return 'link' in fields ? encodeTransactionRequestURL(fields) : encodeTransferRequestURL(fields);
}

function encodeTransactionRequestURL({ link, label, message }: TransactionRequestURLFields): URL {
    // Remove trailing slashes
    const pathname = link.search
        ? encodeURIComponent(String(link).replace(/\/\?/, '?'))
        : String(link).replace(/\/$/, '');
    const url = new URL(SOLANA_PROTOCOL + pathname);

    if (label) {
        url.searchParams.append('label', label);
    }

    if (message) {
        url.searchParams.append('message', message);
    }

    return url;
}

function encodeTransferRequestURL({
    recipient,
    amount,
    splToken,
    reference,
    label,
    message,
    memo,
}: TransferRequestURLFields): URL {
    const pathname = recipient;
    const url = new URL(SOLANA_PROTOCOL + pathname);

    if (amount != null) {
        url.searchParams.append('amount', amount.toFixed(10).replace(/0+$/, '').replace(/\.$/, ''));
    }

    if (splToken) {
        url.searchParams.append('spl-token', splToken);
    }

    const refs = normalizeReferences(reference);
    if (refs) {
        for (const ref of refs) {
            url.searchParams.append('reference', ref);
        }
    }

    if (label) {
        url.searchParams.append('label', label);
    }

    if (message) {
        url.searchParams.append('message', message);
    }

    if (memo) {
        url.searchParams.append('memo', memo);
    }

    return url;
}
