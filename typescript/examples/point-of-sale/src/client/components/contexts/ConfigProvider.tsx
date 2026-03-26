import { PublicKey } from '@solana/web3.js';
import React, { FC, ReactElement, ReactNode } from 'react';
import { ConfigContext } from '../../hooks/useConfig';
import { Confirmations, Digits } from '../../types';

export interface ConfigProviderProps {
    children: ReactNode;
    baseURL: string;
    link?: URL;
    recipient: PublicKey;
    label: string;
    message?: string;
    splToken?: PublicKey;
    symbol: string;
    icon: ReactElement;
    decimals: Digits;
    minDecimals?: Digits;
    requiredConfirmations?: Confirmations;
    connectWallet?: boolean;
}

export const ConfigProvider: FC<ConfigProviderProps> = ({
    children,
    baseURL,
    link,
    recipient,
    label,
    message,
    splToken,
    icon,
    symbol,
    decimals,
    minDecimals = 0,
    requiredConfirmations = 1,
    connectWallet = false,
}) => {
    return (
        <ConfigContext.Provider
            value={{
                baseURL,
                link,
                recipient,
                label,
                message,
                splToken,
                symbol,
                icon,
                decimals,
                minDecimals,
                requiredConfirmations,
                connectWallet,
            }}
        >
            {children}
        </ConfigContext.Provider>
    );
};
