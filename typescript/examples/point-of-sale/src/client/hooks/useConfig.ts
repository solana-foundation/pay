import { PublicKey } from '@solana/web3.js';
import { createContext, ReactElement, useContext } from 'react';
import { Confirmations, Digits } from '../types';

export interface ConfigContextState {
    baseURL: string;
    link: URL | undefined;
    recipient: PublicKey;
    label: string;
    message?: string;
    splToken: PublicKey | undefined;
    symbol: string;
    icon: ReactElement;
    decimals: Digits;
    minDecimals: Digits;
    requiredConfirmations: Confirmations;
    connectWallet: boolean;
}

export const ConfigContext = createContext<ConfigContextState>({} as ConfigContextState);

export function useConfig(): ConfigContextState {
    return useContext(ConfigContext);
}
