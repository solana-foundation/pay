import { createQR, encodeURL, TransactionRequestURLFields } from '@solana/pay'
import { useEffect, useRef } from 'react'
import { useAccount, useConnectWallet, useWalletConnectors, useKitTransactionSigner } from '@solana/connector/react';
import { getTransactionDecoder, getBase64EncodedWireTransaction, createSolanaRpc } from '@solana/kit';
import { useCluster } from '@solana/connector/react';
import {
  PostResponse as CheckoutPostResponse,
  PostError as CheckoutPostError,
} from './api/checkout'

export default function Home() {
  const { address, connected } = useAccount()
  const { signer } = useKitTransactionSigner()
  const { connect } = useConnectWallet()
  const connectors = useWalletConnectors()
  const { cluster } = useCluster()

  const mintQrRef = useRef<HTMLDivElement>(null)

  // Generate the Solana Pay QR code
  useEffect(() => {
    const { location } = window
    const apiUrl = `${location.protocol}//${location.host}/api/checkout`

    const mintUrlFields: TransactionRequestURLFields = {
      link: new URL(apiUrl),
    }
    const mintUrl = encodeURL(mintUrlFields)
    const mintQr = createQR(mintUrl, 400, 'transparent')

    if (mintQrRef.current) {
      mintQrRef.current.innerHTML = ''
      mintQr.append(mintQrRef.current)
    }
  }, [])

  // Handler for performing the transaction with a connected wallet
  async function buy(e: React.MouseEvent) {
    e.preventDefault();

    if (!address || !signer) return;

    const response = await fetch('/api/checkout', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ account: address })
    });

    const responseBody = await response.json() as CheckoutPostResponse | CheckoutPostError;

    if ('error' in responseBody) {
      const error = responseBody.error
      console.error(error)
      alert(`Error fetching transaction: ${error}`)
      return
    }

    // Decode the base64 transaction, sign with kit signer, and send
    const txBytes = Uint8Array.from(atob(responseBody.transaction), c => c.charCodeAt(0));
    const transaction = getTransactionDecoder().decode(txBytes);
    try {
      const [signedTx] = await signer.modifyAndSignTransactions([transaction]);
      const wireBase64 = getBase64EncodedWireTransaction(signedTx);
      const rpc = createSolanaRpc(cluster?.url ?? 'https://api.devnet.solana.com');
      await rpc.sendTransaction(wireBase64, { encoding: 'base64' }).send();
      alert('Purchase complete!')
    } catch (error) {
      console.error(error)
      alert(`Error sending transaction: ${error}`)
    }
  }

  return (
    <main className="container flex flex-col gap-20 items-center p-4 mx-auto min-h-screen justify-center">
      <div className="flex flex-col gap-8">
        <h1 className="text-3xl">Buy in your browser...</h1>
        <div className="basis-1/4">
          {connected && address ? (
            <span>{address.slice(0, 4)}...{address.slice(-4)}</span>
          ) : connectors.length > 0 ? (
            <button
              className="inline-flex items-center rounded-md border border-transparent bg-purple-600 px-4 py-2 text-base font-medium text-white shadow-sm hover:bg-purple-700"
              onClick={() => connect(connectors[0].id)}
            >
              Connect Wallet
            </button>
          ) : null}
        </div>
        <button
          type="button"
          className="max-w-fit inline-flex items-center rounded-md border border-transparent bg-indigo-600 px-6 py-3 text-base font-medium text-white shadow-sm hover:bg-indigo-700 disabled:opacity-50 disabled:cursor-not-allowed"
          disabled={!connected}
          onClick={buy}
        >
          Buy now
        </button>
      </div>

      <div className="flex flex-col gap-8">
        <h1 className="text-3xl">Or scan QR code</h1>
        <div ref={mintQrRef} />
      </div>
    </main>
  )
}
