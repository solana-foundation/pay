import Head from 'next/head'
import { AppProps } from 'next/app'
import React from 'react';
import { ConnectionProvider, WalletProvider } from '@solana/wallet-adapter-react';
import { WalletModalProvider } from '@solana/wallet-adapter-react-ui';
import { clusterApiUrl } from '@solana/web3.js';
import '../styles/index.css'

// Default styles that can be overridden by your app
require('@solana/wallet-adapter-react-ui/styles.css');

function MyApp({ Component, pageProps }: AppProps) {
  // Connection endpoint, switch to a mainnet RPC if using mainnet
  const ENDPOINT = clusterApiUrl('devnet')

  return (
    <>
      <Head>
        <title>Mint your golden ticket</title>
        <meta name="viewport" content="initial-scale=1.0, width=device-width" />
      </Head>
      <ConnectionProvider endpoint={ENDPOINT}>
        <WalletProvider wallets={[]} autoConnect>
          <WalletModalProvider>
            <Component {...pageProps} />
          </WalletModalProvider>
        </WalletProvider>
      </ConnectionProvider>

    </>
  )
}

export default MyApp
