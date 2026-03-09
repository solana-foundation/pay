import { findAssociatedTokenPda, TOKEN_PROGRAM_ADDRESS } from '@solana-program/token';
import type { Address, Signature } from '@solana/kit';
import { createSolanaRpc } from '@solana/kit';
import React, { FC, ReactNode, useEffect, useMemo, useState } from 'react';
import { useConfig } from '../../hooks/useConfig';
import { Transaction, TransactionsContext, TransactionConfirmationStatus } from '../../hooks/useTransactions';
import { Confirmations } from '../../types';
import { arraysEqual } from '../../utils/arraysEqual';
import { DEVNET_ENDPOINT, MAX_CONFIRMATIONS } from '../../utils/constants';

const LAMPORTS_PER_SOL = 1_000_000_000;

/** Minimal shape for accessing jsonParsed transaction fields from the RPC */
interface ParsedTransactionShape {
    message?: {
        readonly instructions?: ReadonlyArray<{
            program?: string;
            parsed?: {
                type: string;
                info: Record<string, unknown>;
            };
        }>;
        readonly accountKeys?: ReadonlyArray<{
            pubkey?: string;
        }>;
    };
}

export interface TransactionsProviderProps {
    children: ReactNode;
    pollInterval?: number;
}

export const TransactionsProvider: FC<TransactionsProviderProps> = ({ children, pollInterval }) => {
    pollInterval ||= 10000;

    const rpc = useMemo(() => createSolanaRpc(DEVNET_ENDPOINT), []);
    const { recipient, splToken } = useConfig();
    const [associatedToken, setAssociatedToken] = useState<Address>();
    const [signatures, setSignatures] = useState<Signature[]>([]);
    const [transactions, setTransactions] = useState<Transaction[]>([]);
    const [loading, setLoading] = useState(false);

    // Get the ATA for the recipient and token
    useEffect(() => {
        if (!splToken) {
            return;
        }

        let changed = false;

        (async () => {
            const [ata] = await findAssociatedTokenPda({
                owner: recipient,
                tokenProgram: TOKEN_PROGRAM_ADDRESS,
                mint: splToken,
            });
            if (changed) return;

            setAssociatedToken(ata);
        })();

        return () => {
            changed = true;
            setAssociatedToken(undefined);
        };
    }, [splToken, recipient]);

    // Poll for signatures referencing the associated token account
    useEffect(() => {
        let changed = false;

        const run = async () => {
            try {
                setLoading(true);

                const confirmedSignatureInfos = await rpc
                    .getSignaturesForAddress(associatedToken || recipient, { limit: 10, commitment: 'confirmed' })
                    .send();
                if (changed) return;

                setSignatures((prevSignatures) => {
                    const nextSignatures = confirmedSignatureInfos.map(({ signature }) => signature as Signature);
                    return arraysEqual(prevSignatures, nextSignatures) ? prevSignatures : nextSignatures;
                });
            } catch (error) {
                console.error(error);
            } finally {
                setLoading(false);
            }
        };

        const interval = setInterval(run, 5000);
        void run();

        return () => {
            changed = true;
            clearInterval(interval);
            setSignatures([]);
        };
    }, [rpc, associatedToken, recipient]);

    // When the signatures change, poll and update the transactions
    useEffect(() => {
        if (!signatures.length) return;
        let changed = false;

        const run = async () => {
            try {
                setLoading(true);

                const [parsedTransactions, signatureStatuses] = await Promise.all([
                    Promise.all(
                        signatures.map((sig) =>
                            rpc.getTransaction(sig, { maxSupportedTransactionVersion: 0, encoding: 'jsonParsed' }).send()
                        )
                    ),
                    rpc.getSignatureStatuses(signatures, { searchTransactionHistory: true }).send(),
                ]);

                if (changed) return;

                setTransactions(
                    signatures
                        .map((sig, signatureIndex): Transaction | undefined => {
                            const parsedTransaction = parsedTransactions[signatureIndex];
                            const signatureStatus = signatureStatuses.value[signatureIndex];
                            if (!parsedTransaction?.meta || !signatureStatus) return;

                            const timestamp = parsedTransaction.blockTime;
                            const error = parsedTransaction.meta.err;
                            const status = signatureStatus.confirmationStatus as TransactionConfirmationStatus;
                            if (!timestamp || !status) return;

                            const instructions = (parsedTransaction.transaction as unknown as ParsedTransactionShape).message?.instructions;
                            if (!instructions || instructions.length !== 1) return;
                            const instruction = instructions[0];
                            if (!('program' in instruction)) return;
                            const program = instruction.program;
                            const type = instruction.parsed?.type;
                            const info = instruction.parsed?.info;

                            let preAmount: number, postAmount: number;
                            if (!associatedToken) {
                                if (!(program === 'system' && type === 'transfer')) return;
                                if (info?.destination !== recipient) return;
                                if (info.source === recipient) return;

                                const accountKeys = (parsedTransaction.transaction as unknown as ParsedTransactionShape).message?.accountKeys;
                                const accountIndex = accountKeys?.findIndex(
                                    (k: { pubkey?: string }) => k.pubkey === recipient
                                );
                                if (accountIndex === -1 || accountIndex == null) return;

                                const preBalance = parsedTransaction.meta.preBalances[accountIndex];
                                const postBalance = parsedTransaction.meta.postBalances[accountIndex];

                                preAmount = Number(preBalance) / LAMPORTS_PER_SOL;
                                postAmount = Number(postBalance) / LAMPORTS_PER_SOL;
                            } else {
                                if (!(program === 'spl-token' && (type === 'transfer' || type === 'transferChecked')))
                                    return;
                                if (info?.destination !== associatedToken) return;
                                if (info.source === associatedToken) return;

                                const accountKeys = (parsedTransaction.transaction as unknown as ParsedTransactionShape).message?.accountKeys;
                                const accountIndex = accountKeys?.findIndex(
                                    (k: { pubkey?: string }) => k.pubkey === associatedToken
                                );
                                if (accountIndex === -1 || accountIndex == null) return;

                                const preBalance = parsedTransaction.meta.preTokenBalances?.find(
                                    (x: { accountIndex: number }) => x.accountIndex === accountIndex
                                );
                                if (!preBalance?.uiTokenAmount?.uiAmountString) return;

                                const postBalance = parsedTransaction.meta.postTokenBalances?.find(
                                    (x: { accountIndex: number }) => x.accountIndex === accountIndex
                                );
                                if (!postBalance?.uiTokenAmount?.uiAmountString) return;

                                preAmount = parseFloat(preBalance.uiTokenAmount.uiAmountString);
                                postAmount = parseFloat(postBalance.uiTokenAmount.uiAmountString);
                            }

                            if (postAmount < preAmount) return;

                            const amount = String(postAmount - preAmount);
                            const confirmations =
                                status === 'finalized'
                                    ? MAX_CONFIRMATIONS
                                    : ((signatureStatus.confirmations || 0) as Confirmations);

                            return {
                                signature: sig,
                                amount,
                                timestamp: Number(timestamp),
                                error,
                                status,
                                confirmations,
                            };
                        })
                        .filter((transaction): transaction is Transaction => !!transaction)
                );
            } catch (error) {
                if (changed) return;
                console.error(error);
            } finally {
                setLoading(false);
            }
        };

        const interval = setInterval(run, pollInterval);
        void run();

        return () => {
            changed = true;
            clearInterval(interval);
        };
    }, [signatures, rpc, associatedToken, recipient, pollInterval]);

    return <TransactionsContext.Provider value={{ transactions, loading }}>{children}</TransactionsContext.Provider>;
};
