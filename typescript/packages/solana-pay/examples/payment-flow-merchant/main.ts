import { createMerchantClient } from '@solana/pay';
import { MERCHANT_WALLET } from './constants.js';
import { simulateCheckout } from './simulateCheckout.js';
import { simulateWalletInteraction } from './simulateWalletInteraction.js';

async function main() {
    console.log("Let's simulate a Solana Pay flow ... \n");

    /**
     * 1. Merchant creates a client — no signer needed, just RPC for reading.
     */
    console.log('1. ✅ Establish connection to the cluster');
    const merchant = createMerchantClient({
        rpcUrl: 'http://127.0.0.1:8899',
        rpcSubscriptionsConfig: { url: 'ws://127.0.0.1:8900' },
    })

    /**
     * Simulate a checkout experience
     *
     * Recommendation:
     * `amount` and `reference` should be created in a trusted environment (server).
     * The `reference` should be unique to a single customer session,
     * and will be used to find and validate the payment in the future.
     */
    console.log('\n2. 🛍 Simulate a customer checkout \n');
    const { label, message, memo, amount, reference } = await simulateCheckout();

    /**
     * Create a payment request link
     *
     * Solana Pay uses a standard URL scheme across wallets for native SOL and SPL Token payments.
     * Several parameters are encoded within the link representing an intent to collect payment from a customer.
     */
    console.log('3. 💰 Create a payment request link \n');
    const url = merchant.pay.encodeURL({ recipient: MERCHANT_WALLET, amount, reference, label, message, memo });

    /**
     * Simulate wallet interaction
     *
     * This is only for example purposes. This interaction will be handled by a wallet provider.
     */
    console.log('4. 🔐 Simulate wallet interaction \n');
    simulateWalletInteraction(url);

    /**
     * Wait for payment to be confirmed
     *
     * When a customer approves the payment request in their wallet, this transaction exists on-chain.
     * You can use any references encoded into the payment link to find the exact transaction on-chain.
     *
     * `watchReference` subscribes via WebSocket (logsNotifications) instead of polling,
     * so the merchant is notified instantly when a matching transaction lands.
     */
    console.log('\n5. Watch for the transaction');

    const { signature } = await merchant.pay.watchReference(reference, { commitment: 'confirmed' });
    console.log('\n 🖌  Signature found: ', signature);

    /**
     * Validate transaction
     *
     * Once the `findReference` function returns a signature,
     * it confirms that a transaction with reference to this order has been recorded on-chain.
     *
     * `validateTransfer` allows you to validate that the transaction signature
     * found matches the transaction that you expected.
     */
    console.log('\n6. 🔗 Validate transaction \n');

    try {
        await merchant.pay.validateTransfer(signature, { recipient: MERCHANT_WALLET, amount }, { commitment: 'confirmed' });
        console.log('✅ Payment validated');
        console.log('📦 Ship order to customer');
    } catch (error) {
        console.error('❌ Payment failed', error);
    }
}

main().then(
    () => process.exit(),
    (err) => {
        console.error(err);
        process.exit(-1);
    }
);
