import { createEmptyClient, type DefaultRpcSubscriptionsChannelConfig, type TransactionSigner } from '@solana/kit';
import { planAndSendTransactions } from '@solana/kit-plugin-instruction-plan';
import { payer } from '@solana/kit-plugin-payer';
import { rpc, rpcTransactionPlanExecutor, rpcTransactionPlanner } from '@solana/kit-plugin-rpc';

import { solanaPayMerchant } from './plugins/merchant.js';
import { solanaPayWallet } from './plugins/wallet.js';

/** Configuration for {@link createMerchantClient}. */
export interface MerchantClientConfig {
    /** Solana RPC URL (e.g. 'https://api.mainnet-beta.solana.com'). */
    rpcUrl: string;
    /** Optional RPC subscriptions config (e.g. custom WebSocket URL for localhost). */
    rpcSubscriptionsConfig?: DefaultRpcSubscriptionsChannelConfig<string>;
}

/** The type returned by {@link createMerchantClient}. */
export type MerchantClient = ReturnType<typeof createMerchantClient>;

/**
 * Creates a merchant-side Solana Pay client with RPC and the merchant plugin.
 * No payer required — merchants only encode URLs, generate QR codes, and
 * read from the network to find/validate payments.
 *
 * @example
 * ```ts
 * const merchant = createMerchantClient({
 *   rpcUrl: 'https://api.mainnet-beta.solana.com',
 * });
 *
 * const url = merchant.pay.encodeURL({ recipient, amount: 1.5 });
 * const found = await merchant.pay.findReference(reference);
 * await merchant.pay.validateTransfer(found.signature, { recipient, amount: 1.5 });
 * ```
 */
export function createMerchantClient(config: MerchantClientConfig) {
    return createEmptyClient().use(rpc(config.rpcUrl, config.rpcSubscriptionsConfig)).use(solanaPayMerchant());
}

/** Configuration for {@link createWalletClient}. */
export interface WalletClientConfig {
    /** Solana RPC URL (e.g. 'https://api.mainnet-beta.solana.com'). */
    rpcUrl: string;
    /** Wallet signer that will be used as the default sender / fee payer. */
    payer: TransactionSigner;
    /** Optional RPC subscriptions config (e.g. custom WebSocket URL for localhost). */
    rpcSubscriptionsConfig?: DefaultRpcSubscriptionsChannelConfig<string>;
}

/** The type returned by {@link createWalletClient}. */
export type WalletClient = ReturnType<typeof createWalletClient>;

/**
 * Creates a wallet-side Solana Pay client with RPC, payer, transaction
 * planner/executor, and the wallet plugin. Handles URL parsing, transfer
 * instruction creation, and transaction sending.
 *
 * @example
 * ```ts
 * const wallet = createWalletClient({
 *   rpcUrl: 'https://api.mainnet-beta.solana.com',
 *   payer: myWalletSigner,
 * });
 *
 * const parsed = wallet.pay.parseURL(url);
 * const instructions = await wallet.pay.createTransfer({ recipient, amount: 1.5 });
 * await wallet.sendTransaction(instructions);
 * ```
 */
export function createWalletClient(config: WalletClientConfig) {
    return createEmptyClient()
        .use(rpc(config.rpcUrl, config.rpcSubscriptionsConfig))
        .use(payer(config.payer))
        .use(rpcTransactionPlanner())
        .use(rpcTransactionPlanExecutor())
        .use(planAndSendTransactions())
        .use(solanaPayWallet());
}

/** Configuration for {@link createSolanaPayClient}. */
export interface SolanaPayClientConfig {
    /** Solana RPC URL (e.g. 'https://api.mainnet-beta.solana.com'). */
    rpcUrl: string;
    /** Wallet signer that will be used as the default sender / fee payer. */
    payer: TransactionSigner;
    /** Optional RPC subscriptions config (e.g. custom WebSocket URL for localhost). */
    rpcSubscriptionsConfig?: DefaultRpcSubscriptionsChannelConfig<string>;
}

/** The type returned by {@link createSolanaPayClient}. */
export type SolanaPayClient = ReturnType<typeof createSolanaPayClient>;

/**
 * Creates a combined Solana Pay client with all merchant + wallet methods.
 *
 * @example
 * ```ts
 * const client = createSolanaPayClient({
 *   rpcUrl: 'https://api.mainnet-beta.solana.com',
 *   payer: myWalletSigner,
 * });
 *
 * const url = client.pay.encodeURL({ recipient, amount: 1.5 });
 * const parsed = client.pay.parseURL(url);
 * const instructions = await client.pay.createTransfer({ recipient, amount: 1.5 });
 * ```
 */
export function createSolanaPayClient(config: SolanaPayClientConfig) {
    return createEmptyClient()
        .use(rpc(config.rpcUrl, config.rpcSubscriptionsConfig))
        .use(payer(config.payer))
        .use(rpcTransactionPlanner())
        .use(rpcTransactionPlanExecutor())
        .use(planAndSendTransactions())
        .use(solanaPayMerchant())
        .use(solanaPayWallet());
}
