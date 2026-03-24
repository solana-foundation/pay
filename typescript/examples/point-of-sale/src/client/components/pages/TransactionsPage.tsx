import { NextPage } from 'next';
import { useRouter } from 'next/router';
import React from 'react';
import { BackButton } from '../buttons/BackButton';
import { PoweredBy } from '../sections/PoweredBy';
import { Transactions } from '../sections/Transactions';
import css from './TransactionsPage.module.css';

const TransactionsPage: NextPage = () => {
    const router = useRouter();

    return (
        <div className={css.root}>
            <div className={css.header}>
                <BackButton onClick={router.back}>Back</BackButton>
            </div>
            <div className={css.main}>
                <Transactions />
            </div>
            <PoweredBy />
        </div>
    );
};

export default TransactionsPage;
