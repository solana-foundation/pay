import {
    createTransfer,
    encodeURL,
    fetchTransaction,
    findReference,
    FindReferenceError,
    parseURL,
    validateTransfer,
    ValidateTransferError,
} from '@solana/pay';
import { useAccount, useKitTransactionSigner } from '@solana/connector/react';
import type { Address, Signature } from '@solana/kit';
import {
    address,
    createSolanaRpc,
    createTransactionMessage,
    setTransactionMessageFeePayer,
    setTransactionMessageLifetimeUsingBlockhash,
    appendTransactionMessageInstructions,
    compileTransaction,
    generateKeyPairSigner,
    getBase64EncodedWireTransaction,
} from '@solana/kit';
import { useRouter } from 'next/router';
import React, { FC, ReactNode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useConfig } from '../../hooks/useConfig';
import { useNavigateWithQuery } from '../../hooks/useNavigateWithQuery';
import { PaymentContext, PaymentStatus } from '../../hooks/usePayment';
import { Confirmations } from '../../types';
import { DEVNET_ENDPOINT } from '../../utils/constants';
import { createPaymentSubmission, PaymentSubmissionError } from '../../utils/paymentSubmission';

export interface PaymentProviderProps {
    children: ReactNode;
}

export const PaymentProvider: FC<PaymentProviderProps> = ({ children }) => {
    const { link, recipient, splToken, label, message, requiredConfirmations, connectWallet } = useConfig();
    const { address: walletAddress } = useAccount();
    const { signer: txSigner } = useKitTransactionSigner();

    // Create a direct RPC client for @solana/pay functions
    const rpc = useMemo(() => createSolanaRpc(DEVNET_ENDPOINT), []);

    const router = useRouter();

    const [amount, setAmount] = useState<number | undefined>(() => {
        const { amount } = router.query;
        if (!amount) return undefined;

        const parsed = parseFloat(Array.isArray(amount) ? amount[0] : amount);
        if (isNaN(parsed) || !isFinite(parsed) || parsed <= 0) {
            console.error('Invalid amount', amount);
            return undefined;
        }

        return parsed;
    });

    const [memo, setMemo] = useState<string>();
    const [reference, setReference] = useState<Address>();
    const [signature, setSignature] = useState<Signature>();
    const [submissionError, setSubmissionError] = useState<string>();
    const submissionRef = useRef<{
        reference: Address;
        submit: ReturnType<typeof createPaymentSubmission>;
    }>();
    const [status, setStatus] = useState(PaymentStatus.New);
    const [confirmations, setConfirmations] = useState<Confirmations>(0);
    const navigate = useNavigateWithQuery();
    const progress = useMemo(() => confirmations / requiredConfirmations, [confirmations, requiredConfirmations]);

    const url = useMemo(() => {
        if (link) {
            const url = new URL(String(link));

            url.searchParams.append('recipient', recipient);

            if (amount) {
                url.searchParams.append('amount', String(amount));
            }

            if (splToken) {
                url.searchParams.append('spl-token', splToken);
            }

            if (reference) {
                url.searchParams.append('reference', reference);
            }

            if (memo) {
                url.searchParams.append('memo', memo);
            }

            if (label) {
                url.searchParams.append('label', label);
            }

            if (message) {
                url.searchParams.append('message', message);
            }

            return encodeURL({ link: url });
        } else {
            return encodeURL({
                recipient,
                amount,
                splToken,
                reference,
                label,
                message,
                memo,
            });
        }
    }, [link, recipient, amount, splToken, reference, label, message, memo]);

    const reset = useCallback(() => {
        setAmount(undefined);
        setMemo(undefined);
        setReference(undefined);
        setSignature(undefined);
        setSubmissionError(undefined);
        submissionRef.current = undefined;
        setStatus(PaymentStatus.New);
        setConfirmations(0);
        navigate('/new', true);
    }, [navigate]);

    const generate = useCallback(async () => {
        if (status === PaymentStatus.New && !reference) {
            const keypair = await generateKeyPairSigner();
            setReference(keypair.address);
            setStatus(PaymentStatus.Pending);
            navigate('/pending');
        }
    }, [status, reference, navigate]);

    // A wallet connection can end before its RPC outcome arrives. Keep that
    // outcome attached to this pending invoice, but retire it when the invoice
    // changes, leaves Pending, or the provider unmounts.
    useEffect(() => {
        return () => {
            submissionRef.current = undefined;
        };
    }, [status, reference]);

    // If there's a connected wallet, use it to sign and send the transaction
    useEffect(() => {
        if (status === PaymentStatus.Pending && reference && connectWallet && walletAddress && txSigner) {
            let changed = false;
            if (submissionRef.current?.reference !== reference) {
                submissionRef.current = { reference, submit: createPaymentSubmission() };
            }
            const attempt = submissionRef.current;

            const run = async () => {
                try {
                    await attempt.submit(
                        async () => {
                            const request = parseURL(url);
                            let compiledTx: ReturnType<typeof compileTransaction>;

                            if ('link' in request) {
                                const { link } = request;
                                compiledTx = await fetchTransaction(rpc, address(walletAddress), link);
                            } else {
                                const { recipient, amount, splToken, reference, memo } = request;
                                if (!amount) throw new Error('An amount is required before payment submission');

                                const instructions = await createTransfer(rpc, txSigner, {
                                    recipient,
                                    amount,
                                    splToken,
                                    reference,
                                    memo,
                                });

                                // Build transaction message
                                const { value: latestBlockhash } = await rpc.getLatestBlockhash().send();
                                const txMessage = appendTransactionMessageInstructions(
                                    instructions,
                                    setTransactionMessageLifetimeUsingBlockhash(
                                        latestBlockhash,
                                        setTransactionMessageFeePayer(
                                            address(walletAddress),
                                            createTransactionMessage({ version: 0 }),
                                        ),
                                    ),
                                );

                                compiledTx = compileTransaction(txMessage);
                            }
                            // Preparation may finish after cleanup. Never open
                            // a wallet prompt for the cancelled effect.
                            if (changed) return undefined;
                            const [signedTx] = await txSigner.modifyAndSignTransactions([compiledTx]);
                            return getBase64EncodedWireTransaction(signedTx);
                        },
                        wire => rpc.sendTransaction(wire, { encoding: 'base64' }).send(),
                        () => changed,
                    );
                } catch (error) {
                    console.error(error);
                    if (submissionRef.current === attempt) {
                        setSubmissionError(
                            error instanceof PaymentSubmissionError && error.broadcastAttempted
                                ? 'The payment submission result is unknown. We are still checking for the payment. Check your wallet before starting another payment.'
                                : 'This attempt did not reach transaction submission. Start over to try again.',
                        );
                    }
                }
            };
            const timeout = setTimeout(run, 0);

            return () => {
                changed = true;
                clearTimeout(timeout);
            };
        }
    }, [status, reference, connectWallet, walletAddress, txSigner, url, rpc]);

    // When the status is pending, poll for the transaction using the reference key
    useEffect(() => {
        if (!(status === PaymentStatus.Pending && reference && !signature)) return;
        let changed = false;

        const interval = setInterval(async () => {
            try {
                const sigInfo = await findReference(rpc, reference);

                if (!changed) {
                    clearInterval(interval);
                    setSignature(sigInfo.signature);
                    setStatus(PaymentStatus.Confirmed);
                    navigate('/confirmed', true);
                }
            } catch (error) {
                if (!(error instanceof FindReferenceError)) {
                    console.error(error);
                }
            }
        }, 250);

        return () => {
            changed = true;
            clearInterval(interval);
        };
    }, [status, reference, signature, rpc, navigate]);

    // When the status is confirmed, validate the transaction against the provided params
    useEffect(() => {
        if (!(status === PaymentStatus.Confirmed && signature && amount)) return;
        let changed = false;

        const run = async () => {
            try {
                await validateTransfer(rpc, signature, { recipient, amount, splToken, reference });

                if (!changed) {
                    setStatus(PaymentStatus.Valid);
                }
            } catch (error) {
                if (
                    error instanceof ValidateTransferError &&
                    (error.message === 'not found' || error.message === 'missing meta')
                ) {
                    console.warn(error);
                    timeout = setTimeout(run, 250);
                    return;
                }

                console.error(error);
                setStatus(PaymentStatus.Invalid);
            }
        };
        let timeout = setTimeout(run, 0);

        return () => {
            changed = true;
            clearTimeout(timeout);
        };
    }, [status, signature, amount, rpc, recipient, splToken, reference]);

    // When the status is valid, poll for confirmations until the transaction is finalized
    useEffect(() => {
        if (!(status === PaymentStatus.Valid && signature)) return;
        let changed = false;

        const interval = setInterval(async () => {
            try {
                const response = await rpc.getSignatureStatuses([signature]).send();
                const sigStatus = response.value[0];
                if (!sigStatus) return;
                if (sigStatus.err) throw sigStatus.err;

                if (!changed) {
                    const confirmations = (sigStatus.confirmations || 0) as Confirmations;
                    setConfirmations(confirmations);

                    if (confirmations >= requiredConfirmations || sigStatus.confirmationStatus === 'finalized') {
                        clearInterval(interval);
                        setStatus(PaymentStatus.Finalized);
                    }
                }
            } catch (error) {
                console.log(error);
            }
        }, 250);

        return () => {
            changed = true;
            clearInterval(interval);
        };
    }, [status, signature, rpc, requiredConfirmations]);

    return (
        <PaymentContext.Provider
            value={{
                amount,
                setAmount,
                memo,
                setMemo,
                reference,
                signature,
                submissionError,
                status,
                confirmations,
                progress,
                url,
                reset,
                generate,
            }}
        >
            {children}
        </PaymentContext.Provider>
    );
};
