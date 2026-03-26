import BigNumber from 'bignumber.js';
import React, { FC, useMemo } from 'react';
import { useConfig } from '../../hooks/useConfig';
import { NON_BREAKING_SPACE } from '../../utils/constants';

export interface AmountProps {
    amount: BigNumber | undefined;
    showZero?: boolean;
}

export const Amount: FC<AmountProps> = ({ amount, showZero }) => {
    const { minDecimals } = useConfig();

    const value = useMemo(() => {
        if (!amount) return NON_BREAKING_SPACE;
        if (amount.isGreaterThan(0)) {
            const decimals = amount.decimalPlaces() ?? 0
            return amount.toFormat(decimals < minDecimals ? minDecimals : decimals);
        } else {
            return showZero ? '0' : NON_BREAKING_SPACE;
        }
    }, [amount, minDecimals, showZero]);

    return <span>{value}</span>;
};
