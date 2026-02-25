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
import BigNumber from 'bignumber.js';
import { useRouter } from 'next/router';
import React, { FC, ReactNode, useCallback, useEffect, useMemo, useState } from 'react';
import { useConfig } from '../../hooks/useConfig';
import { useNavigateWithQuery } from '../../hooks/useNavigateWithQuery';
import { PaymentContext, PaymentStatus } from '../../hooks/usePayment';
import { Confirmations } from '../../types';
import { DEVNET_ENDPOINT } from '../../utils/constants';

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

    const [amount, setAmount] = useState<BigNumber | undefined>(() => {
        const { amount } = router.query;
        if (!amount) return undefined;

        const asBigNumber = Array.isArray(amount) ? new BigNumber(amount[0]) : new BigNumber(amount);
        if (asBigNumber.isNaN() || !asBigNumber.isFinite() || asBigNumber.isLessThanOrEqualTo(0)) {
            console.error('Invalid amount', amount);
            return undefined;
        }

        return asBigNumber;
    });

    const [memo, setMemo] = useState<string>();
    const [reference, setReference] = useState<Address>();
    const [signature, setSignature] = useState<Signature>();
    const [status, setStatus] = useState(PaymentStatus.New);
    const [confirmations, setConfirmations] = useState<Confirmations>(0);
    const navigate = useNavigateWithQuery();
    const progress = useMemo(() => confirmations / requiredConfirmations, [confirmations, requiredConfirmations]);

    const url = useMemo(() => {
        if (link) {
            const url = new URL(String(link));

            url.searchParams.append('recipient', recipient);

            if (amount) {
                url.searchParams.append('amount', amount.toFixed(amount.decimalPlaces() ?? 0));
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

    // If there's a connected wallet, use it to sign and send the transaction
    useEffect(() => {
        if (status === PaymentStatus.Pending && connectWallet && walletAddress && txSigner) {
            let changed = false;

            const run = async () => {
                try {
                    const request = parseURL(url);

                    if ('link' in request) {
                        const { link } = request;
                        const compiledTx = await fetchTransaction(rpc, address(walletAddress), link);
                        if (!changed) {
                            const [signedTx] = await txSigner.modifyAndSignTransactions([compiledTx]);
                            const wireBase64 = getBase64EncodedWireTransaction(signedTx);
                            await rpc.sendTransaction(wireBase64, { encoding: 'base64' }).send();
                        }
                    } else {
                        const { recipient, amount, splToken, reference, memo } = request;
                        if (!amount) return;

                        // createTransfer now returns Instruction[]
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
                                    createTransactionMessage({ version: 0 })
                                )
                            )
                        );

                        const compiled = compileTransaction(txMessage);
                        if (!changed) {
                            const [signedTx] = await txSigner.modifyAndSignTransactions([compiled]);
                            const wireBase64 = getBase64EncodedWireTransaction(signedTx);
                            await rpc.sendTransaction(wireBase64, { encoding: 'base64' }).send();
                        }
                    }
                } catch (error) {
                    console.error(error);
                    timeout = setTimeout(run, 5000);
                }
            };
            let timeout = setTimeout(run, 0);

            return () => {
                changed = true;
                clearTimeout(timeout);
            };
        }
    }, [status, connectWallet, walletAddress, txSigner, url, rpc]);

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
