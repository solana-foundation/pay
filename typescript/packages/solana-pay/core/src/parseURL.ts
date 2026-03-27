import { address } from '@solana/kit';

import { HTTPS_PROTOCOL, SOLANA_PROTOCOL } from './constants.js';
import type { Amount, Label, Link, Memo, Message, Recipient, Reference, SPLToken } from './types.js';

/**
 * A Solana Pay transaction request URL.
 */
export interface TransactionRequestURL {
    /** `link` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#link). */
    link: Link;
    /** `label` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#label-1). */
    label: Label | undefined;
    /** `message` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#message-1). */
    message: Message | undefined;
}

/**
 * A Solana Pay transfer request URL.
 */
export interface TransferRequestURL {
    /** `recipient` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#recipient). */
    recipient: Recipient;
    /** `amount` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#amount). */
    amount: Amount | undefined;
    /** `spl-token` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#spl-token). */
    splToken: SPLToken | undefined;
    /** `reference` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#reference). */
    reference: Reference[] | undefined;
    /** `label` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#label). */
    label: Label | undefined;
    /** `message` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#message). */
    message: Message | undefined;
    /** `memo` in the [Solana Pay spec](https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#memo). */
    memo: Memo | undefined;
}

/**
 * Thrown when a URL can't be parsed as a Solana Pay URL.
 */
export class ParseURLError extends Error {
    name = 'ParseURLError';
}

/**
 * Parse a Solana Pay URL.
 *
 * @param url - URL to parse.
 *
 * @throws {ParseURLError}
 */
export function parseURL(url: URL | string): TransactionRequestURL | TransferRequestURL {
    if (typeof url === 'string') {
        if (url.length > 2048) throw new ParseURLError('length invalid');
        url = new URL(url);
    }

    if (url.protocol !== SOLANA_PROTOCOL) throw new ParseURLError('protocol invalid');
    if (!url.pathname) throw new ParseURLError('pathname missing');

    return /[:%]/.test(url.pathname) ? parseTransactionRequestURL(url) : parseTransferRequestURL(url);
}

function parseTransactionRequestURL({ pathname, searchParams }: URL): TransactionRequestURL {
    const link = new URL(decodeURIComponent(pathname));
    if (link.protocol !== HTTPS_PROTOCOL) throw new ParseURLError('link invalid');

    const label = searchParams.get('label') || undefined;
    const message = searchParams.get('message') || undefined;

    return {
        link,
        label,
        message,
    };
}

function parseTransferRequestURL({ pathname, searchParams }: URL): TransferRequestURL {
    let recipient: Recipient;
    try {
        recipient = address(pathname);
    } catch {
        throw new ParseURLError('recipient invalid');
    }

    let amount: number | undefined;
    const amountParam = searchParams.get('amount');
    if (amountParam != null) {
        if (!/^\d+(\.\d+)?$/.test(amountParam)) throw new ParseURLError('amount invalid');

        amount = Number(amountParam);
        if (Number.isNaN(amount)) throw new ParseURLError('amount NaN');
        if (amount < 0) throw new ParseURLError('amount negative');
    }

    let splToken: SPLToken | undefined;
    const splTokenParam = searchParams.get('spl-token');
    if (splTokenParam != null) {
        try {
            splToken = address(splTokenParam);
        } catch {
            throw new ParseURLError('spl-token invalid');
        }
    }

    let reference: Reference[] | undefined;
    const referenceParams = searchParams.getAll('reference');
    if (referenceParams.length) {
        try {
            reference = referenceParams.map(ref => address(ref));
        } catch {
            throw new ParseURLError('reference invalid');
        }
    }

    const label = searchParams.get('label') || undefined;
    const message = searchParams.get('message') || undefined;
    const memo = searchParams.get('memo') || undefined;

    return {
        recipient,
        amount,
        splToken,
        reference,
        label,
        message,
        memo,
    };
}
