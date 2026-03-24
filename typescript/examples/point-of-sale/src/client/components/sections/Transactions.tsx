import BigNumber from 'bignumber.js';
import clsx from 'clsx';
import { formatDistance } from 'date-fns';
import React, { FC, useCallback, useEffect, useMemo, useState } from 'react';
import { useConfig } from '../../hooks/useConfig';
import { Transaction, useTransactions } from '../../hooks/useTransactions';
import { NON_BREAKING_SPACE } from '../../utils/constants';
import { Amount } from './Amount';
import css from './Transactions.module.css';

export const Transactions: FC = () => {
    const { transactions } = useTransactions();

    return (
        <div className={css.root}>
            <div className={css.title}>Recent Transactions</div>
            {transactions.map((transaction) => (
                <Transaction key={transaction.signature} transaction={transaction} />
            ))}
        </div>
    );
};

const Transaction: FC<{ transaction: Transaction }> = ({ transaction }) => {
    const { icon, symbol } = useConfig();

    const amount = useMemo(() => new BigNumber(transaction.amount), [transaction.amount]);
    const signature = useMemo(
        () => transaction.signature.slice(0, 8) + '....' + transaction.signature.slice(-8),
        [transaction.signature]
    );

    const getTime = useCallback(
        () => formatDistance(new Date(), new Date(transaction.timestamp * 1000)) + ' ago',
        [transaction.timestamp]
    );
    const [time, setTime] = useState(getTime());
    useEffect(() => {
        const interval = setInterval(() => setTime(getTime()), 1000);
        return () => clearInterval(interval);
    }, [getTime]);

    return (
        <div className={css.transaction}>
            <div className={css.icon}>{icon}</div>
            <div className={css.left}>
                <div className={css.amount}>
                    <Amount amount={amount} showZero />
                    {NON_BREAKING_SPACE + symbol}
                </div>
                <div className={css.signature}>{signature}</div>
            </div>
            <div className={css.right}>
                <div className={css.time}>{time}</div>
                <div className={clsx(css.status, css[`status-${transaction.status}`])}>{transaction.status}</div>
            </div>
        </div>
    );
};
