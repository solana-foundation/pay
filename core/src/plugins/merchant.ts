import type { GetSignaturesForAddressApi, GetTransactionApi, Signature } from '@solana/kit';
import type { ClientWithRpc } from '@solana/plugin-interfaces';

import { createQR, createQROptions } from '../createQR.js';
import type { TransactionRequestURLFields, TransferRequestURLFields } from '../encodeURL.js';
import { encodeURL } from '../encodeURL.js';
import type { ConfirmedSignatureInfo, FindReferenceOptions } from '../findReference.js';
import { findReference } from '../findReference.js';
import type { Finality, Reference, TransferFields } from '../types.js';
import { validateTransfer } from '../validateTransfer.js';

type MerchantRpcApi = GetSignaturesForAddressApi & GetTransactionApi;

type MerchantCompatibleClient = ClientWithRpc<MerchantRpcApi>;

/** Methods added to a client by the merchant plugin. */
export interface SolanaPayMerchantMethods {
    readonly pay: {
        encodeURL(fields: TransactionRequestURLFields | TransferRequestURLFields): URL;
        createQR(url: URL | string, size?: number, background?: string, color?: string): any;
        createQROptions(
            url: URL | string,
            size?: number,
            background?: string,
            color?: string,
        ): ReturnType<typeof createQROptions>;
        findReference(reference: Reference, options?: FindReferenceOptions): Promise<ConfirmedSignatureInfo>;
        validateTransfer(
            signature: Signature,
            fields: TransferFields,
            options?: { commitment?: Finality },
        ): Promise<Awaited<ReturnType<typeof validateTransfer>>>;
    };
}

/**
 * Merchant plugin for Solana Pay.
 *
 * Adds a `pay` namespace with merchant-side methods: URL encoding,
 * QR code generation, reference lookup, and transfer validation.
 * No payer/signer required — merchant only reads from the network.
 */
export function solanaPayMerchant() {
    return function installMerchant<TClient extends MerchantCompatibleClient>(
        client: TClient,
    ): SolanaPayMerchantMethods & TClient {
        const existingPay = 'pay' in client ? (client as { pay: Record<string, unknown> }).pay : {};
        const pay: SolanaPayMerchantMethods['pay'] = {
            ...existingPay,
            encodeURL(fields: TransactionRequestURLFields | TransferRequestURLFields): URL {
                return encodeURL(fields);
            },
            createQR(url: URL | string, size?: number, background?: string, color?: string): any {
                return createQR(url, size, background, color);
            },
            createQROptions(url: URL | string, size?: number, background?: string, color?: string) {
                return createQROptions(url, size, background, color);
            },
            async findReference(reference: Reference, options?: FindReferenceOptions): Promise<ConfirmedSignatureInfo> {
                return await findReference(client.rpc, reference, options);
            },
            async validateTransfer(signature: Signature, fields: TransferFields, options?: { commitment?: Finality }) {
                return await validateTransfer(client.rpc, signature, fields, options);
            },
        };

        return Object.freeze({ ...client, pay }) as SolanaPayMerchantMethods & TClient;
    };
}
