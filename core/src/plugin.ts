import type {
    Address,
    Commitment,
    GetAccountInfoApi,
    GetLatestBlockhashApi,
    GetSignaturesForAddressApi,
    GetTransactionApi,
    Instruction,
    Rpc,
    Signature,
    TransactionSigner,
} from '@solana/kit';
import { createTransfer } from './createTransfer.js';
import type { CreateTransferFields } from './createTransfer.js';
import { encodeURL } from './encodeURL.js';
import type { TransactionRequestURLFields, TransferRequestURLFields } from './encodeURL.js';
import { parseURL } from './parseURL.js';
import type { TransactionRequestURL, TransferRequestURL } from './parseURL.js';
import { createQR, createQROptions } from './createQR.js';
import { findReference } from './findReference.js';
import type { FindReferenceOptions, ConfirmedSignatureInfo } from './findReference.js';
import { validateTransfer } from './validateTransfer.js';
import type { ValidateTransferFields } from './validateTransfer.js';
import { fetchTransaction } from './fetchTransaction.js';
import type { FetchedTransaction } from './fetchTransaction.js';
import type { Finality, Reference } from './types.js';

/**
 * RPC API methods required by the Solana Pay plugin.
 *
 * A client that installs this plugin must have an `rpc` property
 * supporting at least these API methods.
 */
type SolanaPayRpcApi = GetAccountInfoApi & GetLatestBlockhashApi & GetSignaturesForAddressApi & GetTransactionApi;

/**
 * The shape of a client that can accept the Solana Pay plugin.
 * Must have `rpc` and optionally `payer` (a {@link TransactionSigner}).
 */
interface SolanaPayCompatibleClient {
    readonly rpc: Rpc<SolanaPayRpcApi>;
    readonly payer?: TransactionSigner;
}

/**
 * Methods added to a client by the Solana Pay plugin.
 */
export interface SolanaPayMethods {
    readonly pay: {
        /**
         * Create transfer instructions for a Solana Pay transfer request.
         * If `sender` is omitted, falls back to `client.payer`.
         */
        createTransfer(fields: CreateTransferFields, sender?: TransactionSigner): Promise<Instruction[]>;

        /**
         * Encode a Solana Pay URL from transfer or transaction request fields.
         */
        encodeURL(fields: TransactionRequestURLFields | TransferRequestURLFields): URL;

        /**
         * Parse a Solana Pay URL into its constituent fields.
         */
        parseURL(url: string | URL): TransactionRequestURL | TransferRequestURL;

        /**
         * Create a QR code for a Solana Pay URL.
         * @returns A QRCodeStyling instance (typed as `any` due to CJS/ESM compat).
         */
        createQR(url: string | URL, size?: number, background?: string, color?: string): any;

        /**
         * Create QR code options without instantiating the QR code.
         */
        createQROptions(
            url: string | URL,
            size?: number,
            background?: string,
            color?: string
        ): ReturnType<typeof createQROptions>;

        /**
         * Find a transaction signature referencing the given address.
         */
        findReference(reference: Reference, options?: FindReferenceOptions): Promise<ConfirmedSignatureInfo>;

        /**
         * Validate that a confirmed transaction contains the expected Solana Pay transfer.
         */
        validateTransfer(
            signature: Signature,
            fields: ValidateTransferFields,
            options?: { commitment?: Finality }
        ): Promise<Awaited<ReturnType<typeof validateTransfer>>>;

        /**
         * Fetch a transaction from a Solana Pay transaction request endpoint.
         */
        fetchTransaction(
            account: Address,
            link: string | URL,
            options?: { commitment?: Commitment }
        ): Promise<FetchedTransaction>;
    };
}

/**
 * Solana Pay plugin for `@solana/kit` clients.
 *
 * Adds a `pay` namespace to the client with all Solana Pay functions pre-bound
 * to the client's RPC and payer.
 *
 * @example
 * ```ts
 * import { createEmptyClient } from '@solana/kit';
 * import { rpc } from '@solana/kit-plugins';
 * import { solanaPay } from '@solana/pay';
 *
 * const client = createEmptyClient()
 *   .use(rpc('https://api.devnet.solana.com'))
 *   .use(solanaPay());
 *
 * const url = client.pay.encodeURL({ recipient, amount });
 * const instructions = await client.pay.createTransfer({ recipient, amount });
 * ```
 */
export function solanaPay() {
    return function installSolanaPay<TClient extends SolanaPayCompatibleClient>(
        client: TClient
    ): TClient & SolanaPayMethods {
        const pay: SolanaPayMethods['pay'] = {
            async createTransfer(fields: CreateTransferFields, sender?: TransactionSigner): Promise<Instruction[]> {
                const signer = sender ?? client.payer;
                if (!signer) {
                    throw new Error('solanaPay.createTransfer requires a sender or client.payer');
                }
                return createTransfer(client.rpc, signer, fields);
            },
            encodeURL(fields: TransactionRequestURLFields | TransferRequestURLFields): URL {
                return encodeURL(fields);
            },
            parseURL(url: string | URL): TransactionRequestURL | TransferRequestURL {
                return parseURL(url);
            },
            createQR(url: string | URL, size?: number, background?: string, color?: string): any {
                return createQR(url, size, background, color);
            },
            createQROptions(url: string | URL, size?: number, background?: string, color?: string) {
                return createQROptions(url, size, background, color);
            },
            async findReference(reference: Reference, options?: FindReferenceOptions): Promise<ConfirmedSignatureInfo> {
                return findReference(client.rpc, reference, options);
            },
            async validateTransfer(
                signature: Signature,
                fields: ValidateTransferFields,
                options?: { commitment?: Finality }
            ) {
                return validateTransfer(client.rpc, signature, fields, options);
            },
            async fetchTransaction(
                account: Address,
                link: string | URL,
                options?: { commitment?: Commitment }
            ): Promise<FetchedTransaction> {
                return fetchTransaction(client.rpc, account, link, options);
            },
        };

        return Object.freeze({ ...client, pay }) as TClient & SolanaPayMethods;
    };
}
