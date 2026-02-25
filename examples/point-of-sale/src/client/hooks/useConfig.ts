import type { Address } from '@solana/kit';
import { createContext, ReactElement, useContext } from 'react';
import { Confirmations, Digits } from '../types';

export interface ConfigContextState {
    baseURL: string;
    link: URL | undefined;
    recipient: Address;
    label: string;
    message?: string;
    splToken: Address | undefined;
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
