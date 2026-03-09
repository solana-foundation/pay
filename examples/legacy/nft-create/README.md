# Send an NFT as part of a Solana Pay payment

This repo demonstrates an example of the following transaction using Solana Pay:

- Buyer pays X USDC 
- They're airdropped an NFT as part of the transaction

It uses the new `@metaplex-foundation/js` library which makes NFT instructions much easier to work with!


## Environment variable

One environment variable is required, `SHOP_PRIVATE_KEY`. This is the base58 encoded private key for an account that will pay the network and storage fees.

Copy `.env.example` to `.env` and set the private key there.

This is required for uploading NFT metadata and for the checkout API.


## Install dependencies

Dependencies are managed with npm. Run `npm install` in this directory to install them.

## Upload NFT metadata

Before an NFT can be created we need to upload its metadata. The easiest way is using Bundlr storage, which Metaplex has a nice plugin for.

Uploading is done using the script [upload.js](./nft-upload/upload.js)

The NFT image file is [pay-logo.svg](./nft-upload/pay-logo.svg) in the same directory. Feel free to change it!

The rest of the NFT metadata is set by variables in `upload.js`.

Once you've set these you can run the script:

```shell
$ node nft-upload/upload.js
Uploaded metadata: https://arweave.net/mAbxQsdFYQNRFPqWHNzZbkwVw6LFp3k9LvRTxuZpVXk
Done!
```

## Running the web app

This example is a NextJS app. Run `npm run dev` to start it on http://localhost:3000. 


## Checkout API

The checkout API takes a public key as input and returns a partially signed transaction, which must be signed by the input public key before it is broadcast.

The checkout API code is at [checkout.ts](./pages/api/checkout.ts)

- You'll need to set the metadata URL to your own, as returned by `nft-upload/upload.js`:
```ts
const METADATA_URI = "..."
```

- You can also set the USDC address (or change the SPL token), the RPC endpoint, the created NFT name and the USDC price in variables at the top of this file. 



It first creates a Metaplex `TransactionBuilder` to create an NFT:

```ts
  const transactionBuilder = await nfts.builders().create({
    uri: METADATA_URI, // use our metadata
    name: NFT_NAME,
    tokenOwner: account, // NFT is minted to the wallet submitting the transaction (buyer)
    updateAuthority: shopKeypair, // we retain update authority
    sellerFeeBasisPoints: 100, // 1% royalty
    useNewMint: mintKeypair, // we pass our mint in as the new mint to use
  })
```

By using a transaction builder we can control the conversion to a Solana `Transaction` and then return it.

It then creates an SPL token transaction to send USDC from the buyer to the shop:

```ts
  const usdcTransferInstruction = createTransferCheckedInstruction(
    fromUsdcAddress.address, // from USDC address
    USDC_ADDRESS, // USDC mint address
    toUsdcAddress.address, // to USDC address
    account, // owner of the from USDC address (the buyer)
    PRICE_USDC * (10 ** decimals), // multiply by 10^decimals
    decimals
  )
```

This instruction is prepended to the NFT transaction, so that it's part of the same atomic transaction.

We then convert it to a `Transaction`, and sign it as:

- The shop keypair, which is our Metaplex identity and pays the fees (so the buyer pays no SOL, just USDC)
- The mint keypair, which we generate in the API and pass to the NFT create function.

This transaction is only **partially signed**, the USDC instruction additionally requires the user's signature.

We return this transaction, and the user's wallet will be able to sign it as them and then submit it to the network.


## Submitting the transaction

The home page is at [index.tsx](./pages/index.tsx). It has code to connect a wallet (using wallet-adapter) and fetch/send the transaction. It also has code to display a QR code that can be scanned by wallets that support Solana Pay, which encodes a call to the checkout API.

Both are an identical transaction. The browser wallets tend to have better error messaging if anything goes wrong, and you'll have access to the browser console too.

### Making localhost:3000 internet accessible

To enable your local development environment to be accessed from the internet—for example, to allow a mobile wallet to resolve and connect to your `localhost` server—you can use secure tunneling tools like Tunnelmole or ngrok.

#### Using Tunnelmole (Open Source)

[Tunnelmole](https://github.com/robbie-cahill/tunnelmole-client) is an open source tunneling tool that allows you to create a Public URL that forwards traffic to your local machine via a secure tunnel. Once installed, start the Tunnelmole service for the desired local port (e.g., 3000):

```
tmole 3000
```

Tunnelmole will present an output like:

```
http://bvdo5f-ip-49-183-170-144.tunnelmole.net is forwarding to localhost:3000
https://bvdo5f-ip-49-183-170-144.tunnelmole.net is forwarding to localhost:3000
```

This will provide you with a Public URL that you can use to make your `localhost:3000` accessible from any device. For instance, it will allow a QR code to encode a URL like `https://bvdo5f-ip-49-183-170-144.tunnelmole.net/api/checkout` which will be resolvable by mobile wallets.

#### Using ngrok (Closed Source)

Alternatively, you can use [ngrok](https://ngrok.com), a popular closed source tunneling tool. After signing up (free) and downloading their CLI, run the following command:

```
ngrok http 3000
```

ngrok will present an output like:

```
Forwarding                    https://6fba-2a02-c7c-50a3-a200-1402-5c1a-a7d2-174d.eu.ngrok.io -> http://localhost:3000
```

This `ngrok.io` domain will forward to your `localhost:3000` and will also be accessible from anywhere. For example, it will make the home page with a QR code that encodes a URL like `https://6fba-2a02-c7c-50a3-a200-1402-5c1a-a7d2-174d.eu.ngrok.io/api/checkout` work correctly with mobile wallets.

By using Tunnelmole or ngrok, you can quickly and securely expose your local development server to the internet for testing and sharing with others.
