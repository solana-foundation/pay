import type { FC } from 'react';
import { useMediaQuery } from 'react-responsive';
import { useLinkWithQuery } from '../../hooks/useLinkWithQuery';
import { ActivityIcon } from '../images/ActivityIcon';
import css from './TransactionsLink.module.css';

export const TransactionsLink: FC = () => {
    const to = useLinkWithQuery('/transactions');
    const phone = useMediaQuery({ query: '(max-width: 767px)' });

    return (
        <a className={css.link} href={to}>
            <ActivityIcon />
            {phone ? null : 'Recent Transactions'}
        </a>
    );
};
