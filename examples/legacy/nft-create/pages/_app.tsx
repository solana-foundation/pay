import Head from 'next/head'
import { AppProps } from 'next/app'
import React from 'react';
import { AppProvider } from '@solana/connector/react';
import { getDefaultConfig } from '@solana/connector/headless';
import '../styles/index.css'

const connectorConfig = getDefaultConfig({
  appName: 'NFT Create',
  autoConnect: true,
  network: 'devnet',
});

function MyApp({ Component, pageProps }: AppProps) {
  return (
    <>
      <Head>
        <title>Mint your golden ticket</title>
        <meta name="viewport" content="initial-scale=1.0, width=device-width" />
      </Head>
      <AppProvider connectorConfig={connectorConfig}>
        <Component {...pageProps} />
      </AppProvider>
    </>
  )
}

export default MyApp
