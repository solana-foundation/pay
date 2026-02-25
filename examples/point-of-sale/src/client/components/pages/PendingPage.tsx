import { useAccount, useConnectWallet, useWalletConnectors } from '@solana/connector/react';
import { NextPage } from 'next';
import React, { useEffect } from 'react';
import { useConfig } from '../../hooks/useConfig';
import { usePayment } from '../../hooks/usePayment';
import { BackButton } from '../buttons/BackButton';
import { Amount } from '../sections/Amount';
import { PoweredBy } from '../sections/PoweredBy';
import { QRCode } from '../sections/QRCode';
import css from './PendingPage.module.css';

const PendingPage: NextPage = () => {
    const { symbol, connectWallet: connectWalletConfig } = useConfig();
    const { amount, reset } = usePayment();
    const { address, connected } = useAccount();
    const { connect } = useConnectWallet();
    const connectors = useWalletConnectors();

    useEffect(() => {
        if (connectWalletConfig && !connected && connectors.length > 0) {
            // Auto-connect to first available wallet
            connect(connectors[0].id);
        }
    }, [connectWalletConfig, connected, connectors, connect]);

    return (
        <div className={css.root}>
            <div className={css.header}>
                <BackButton onClick={reset}>Cancel</BackButton>
                {connectWalletConfig && connectors.length > 0 ? (
                    <button
                        onClick={() => {
                            if (connected) return;
                            connect(connectors[0].id);
                        }}
                    >
                        {connected && address ? `${address.slice(0, 4)}...${address.slice(-4)}` : 'Connect Wallet'}
                    </button>
                ) : null}
            </div>
            <div className={css.main}>
                <div className={css.amount}>
                    <Amount amount={amount} />
                </div>
                <div className={css.symbol}>{symbol}</div>
                <div className={css.code}>
                    <QRCode />
                </div>
                <div className={css.scan}>Scan this code with your Solana Pay wallet</div>
                <div className={css.confirm}>You'll be asked to approve the transaction</div>
            </div>
            <div className={css.footer}>
                <PoweredBy />
            </div>
        </div>
    );
};

export default PendingPage;

export function getServerSideProps() {
    return {
        props: {},
    };
}
