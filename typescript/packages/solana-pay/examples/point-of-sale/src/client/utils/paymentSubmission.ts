import type { Base64EncodedWireTransaction } from '@solana/kit';

/** Whether this attempt reached the RPC submission boundary. */
export class PaymentSubmissionError extends Error {
    constructor(
        readonly broadcastAttempted: boolean,
        readonly cause: unknown,
    ) {
        super(broadcastAttempted ? 'Transaction submission outcome is unknown' : 'Transaction was not submitted');
        this.name = 'PaymentSubmissionError';
        Object.setPrototypeOf(this, new.target.prototype);
    }
}

/** Keep one wallet authorization and at most one broadcast per invoice. */
export function createPaymentSubmission() {
    let signedWire: Promise<Base64EncodedWireTransaction | undefined> | undefined;
    let submission: Promise<void> | undefined;

    return async (
        prepareAndSign: () => Promise<Base64EncodedWireTransaction | undefined>,
        send: (wire: Base64EncodedWireTransaction) => Promise<unknown>,
        cancelled: () => boolean,
    ): Promise<void> => {
        while (!cancelled()) {
            // Share preparation and the wallet prompt. A rejection stays attached
            // to this attempt; restarting an effect must await the same outcome.
            signedWire ??= (async () => {
                try {
                    return await prepareAndSign();
                } catch (error) {
                    throw new PaymentSubmissionError(false, error);
                }
            })();
            const preparation = signedWire;
            const wire = await preparation;
            if (wire === undefined) {
                // The effect was cancelled during preparation, before opening a
                // wallet prompt. An active replacement can prepare its own request.
                if (signedWire === preparation) signedWire = undefined;
                continue;
            }
            if (cancelled()) return;

            // Any failure after calling send is unknown, even if an RPC wrapper
            // claims it rejected before submission. Keep polling the reference.
            submission ??= (async () => {
                try {
                    await send(wire);
                } catch (error) {
                    throw new PaymentSubmissionError(true, error);
                }
            })();
            await submission;
            return;
        }
    };
}
