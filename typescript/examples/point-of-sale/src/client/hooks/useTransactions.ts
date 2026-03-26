import { TransactionConfirmationStatus, TransactionError, TransactionSignature } from '@solana/web3.js';
import { createContext, useContext } from 'react';
import { Confirmations } from '../types';

export interface Transaction {
    signature: TransactionSignature;
    amount: string;
    timestamp: number;
    error: TransactionError | null;
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
