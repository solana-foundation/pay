import type { Signature } from '@solana/kit';
import { createContext, useContext } from 'react';
import { Confirmations } from '../types';

export type TransactionConfirmationStatus = 'processed' | 'confirmed' | 'finalized';

export interface Transaction {
    signature: Signature;
    amount: string;
    timestamp: number;
    error: unknown | null;
    status: TransactionConfirmationStatus;
    confirmations: Confirmations;
}

export interface TransactionsContextState {
    transactions: Transaction[];
    loading: boolean;
}

export const TransactionsContext = createContext<TransactionsContextState>({} as TransactionsContextState);

export function useTransactions(): TransactionsContextState {
    return useContext(TransactionsContext);
}
