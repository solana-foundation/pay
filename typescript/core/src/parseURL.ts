import { PublicKey } from '@solana/web3.js';
import BigNumber from 'bignumber.js';
import { HTTPS_PROTOCOL, SOLANA_PROTOCOL } from './constants.js';
import type { Amount, Label, Link, Memo, Message, Recipient, Reference, SPLToken } from './types.js';

/**
 * A Solana Pay transaction request URL.
 */
export interface TransactionRequestURL {
    /** `link` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#link). */
    link: Link;
    /** `label` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#label-1). */
    label: Label | undefined;
    /** `message` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#message-1). */
    message: Message | undefined;
}

/**
 * A Solana Pay transfer request URL.
 */
export interface TransferRequestURL {
    /** `recipient` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#recipient). */
    recipient: Recipient;
    /** `amount` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#amount). */
    amount: Amount | undefined;
    /** `spl-token` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#spl-token). */
    splToken: SPLToken | undefined;
    /** `reference` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#reference). */
    reference: Reference[] | undefined;
    /** `label` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#label). */
    label: Label | undefined;
    /** `message` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#message). */
    message: Message | undefined;
    /** `memo` in the [Solana Pay spec](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#memo). */
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
export function parseURL(url: string | URL): TransactionRequestURL | TransferRequestURL {
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
    let recipient: PublicKey;
    try {
        recipient = new PublicKey(pathname);
    } catch (error: any) {
        throw new ParseURLError('recipient invalid');
    }

    let amount: BigNumber | undefined;
    const amountParam = searchParams.get('amount');
    if (amountParam != null) {
        if (!/^\d+(\.\d+)?$/.test(amountParam)) throw new ParseURLError('amount invalid');

        amount = new BigNumber(amountParam);
        if (amount.isNaN()) throw new ParseURLError('amount NaN');
        if (amount.isNegative()) throw new ParseURLError('amount negative');
    }

    let splToken: PublicKey | undefined;
    const splTokenParam = searchParams.get('spl-token');
    if (splTokenParam != null) {
        try {
            splToken = new PublicKey(splTokenParam);
        } catch (error) {
            throw new ParseURLError('spl-token invalid');
        }
    }

    let reference: PublicKey[] | undefined;
    const referenceParams = searchParams.getAll('reference');
    if (referenceParams.length) {
        try {
            reference = referenceParams.map((reference) => new PublicKey(reference));
        } catch (error) {
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
