import React, { FC, useMemo } from 'react';
import { useConfig } from '../../hooks/useConfig';
import { NON_BREAKING_SPACE } from '../../utils/constants';

export interface AmountProps {
    amount: number | undefined;
    showZero?: boolean;
}

export const Amount: FC<AmountProps> = ({ amount, showZero }) => {
    const { minDecimals } = useConfig();

    const value = useMemo(() => {
        if (amount == null) return NON_BREAKING_SPACE;
        if (amount > 0) {
            const decimals = (amount.toString().split('.')[1] || '').length;
            return amount.toLocaleString(undefined, {
                minimumFractionDigits: decimals < minDecimals ? minDecimals : decimals,
                maximumFractionDigits: Math.max(decimals, minDecimals),
            });
        } else {
            return showZero ? '0' : NON_BREAKING_SPACE;
        }
    }, [amount, minDecimals, showZero]);

    return <span>{value}</span>;
};
