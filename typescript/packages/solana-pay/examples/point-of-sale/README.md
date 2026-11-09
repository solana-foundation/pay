# Point of Sale

This is an example of how you can use the `@solana/pay` JavaScript library to create a simple point of sale system.

You can [check out the app](https://app.solanapay.com?recipient=GvHeR432g7MjN9uKyX3Dzg66TqwrEWgANLnnFZXMeyyj&label=Solana+Pay), use the code as a reference, or run it yourself to start accepting decentralized payments in-person.

## Prerequisites

To build and run this app locally, you'll need:

-   Node.js v20.18.0 or above
-   Yarn
-   <details>
        <summary> Setup two wallets on <a href="https://phantom.app">Phantom</a> (Merchant and Customer) </summary>

    #### 1. Create merchant wallet

    Follow the [guide][1] on how to create a wallet. This wallet will provide the recipient address.

    #### 2. Create customer wallet

    Follow the [guide][1] on how to create another wallet. This wallet will be paying for the goods/services.

    #### 3. Set Phantom to connect to devnet

    1. Click the settings icon in the Phantom window
    2. Select the "Change network" option and select "Devnet"

    #### 4. Airdrop SOL to customer wallet

    Use [solfaucet][3] to airdrop SOL to the customer wallet.

    > You'll need SOL in the customer wallet to pay for the goods/services + transaction fees

 </details>

## Getting Started

These instructions will get you a copy of the project up and running on your local machine for development and testing purposes.

### Payment retry behavior and local checks

The optional connected-wallet flow keeps one authorization and one submission
attempt for each invoice reference. It does not automatically request a new
signature after a signing or submission error. A lost RPC response does not
prove that the transaction failed, so reference polling continues while the
page explains the uncertainty. Check the wallet before starting another invoice.
Preparation/signing failures are shown separately from unknown submission
outcomes; neither displays another payment QR code on the error screen.

The guard is in memory and ends when the provider is unmounted or reloaded. It
does not coordinate payments sent from a separate wallet through a QR code.
The default configuration uses devnet and leaves `connectWallet` disabled.

The example is outside the SDK's pnpm workspace. Build its local SDK dependency
before installing and checking the example:

```shell
cd typescript
pnpm install --frozen-lockfile --ignore-scripts
pnpm --filter @solana/pay build
cd packages/solana-pay/examples/point-of-sale
npm install --ignore-scripts
npm test
npx --no-install tsc --noEmit --incremental false
npm run build
```

The `POS example` workflow runs these checks explicitly. The regression tests
mount the real provider and pending page, with generated signers and actual SPL
transfers in LiteSVM. Connector/Next hooks, response loss, reference discovery
and transfer validation are controlled fixtures; no real wallet or funds are
used. A full application build checks the real Next and connector dependencies.

### Clone the repository

#### With Git
```shell
git clone https://github.com/solana-labs/solana-pay.git
```

#### With Github CLI
```shell
gh repo clone solana-labs/solana-pay
```

### Install dependencies
```shell
cd solana-pay/examples/point-of-sale
npm install
```

### Start the local dev server
```shell
npm run dev
```

### In a separate terminal, run a local SSL proxy
```shell
npm run proxy
```

### Open the point of sale app

Update `recipient` with your  destination wallet:

```shell
open "https://localhost:3001/new?recipient=11111111111111111111111111111111&label=Test%20Store"
```

You may need to accept a locally signed SSL certificate to open the page.

## Accepting USDC on Mainnet
Import the Mainnet endpoint, along with USDC's mint address and icon in the [`client/components/pages/App.tsx`](https://github.com/solana-labs/solana-pay/blob/master/examples/point-of-sale/src/client/components/pages/App.tsx) file.
```tsx
import { MAINNET_ENDPOINT, MAINNET_USDC_MINT } from '../../utils/constants';
import { USDCIcon } from '../images/USDCIcon';
```

In the same file, set the `endpoint` value in the `<ConnectionProvider>` to `MAINNET_ENDPOINT` and set the following values in the `<ConfigProvider>`:

```tsx
splToken={MAINNET_USDC_MINT}
symbol="USDC"
icon={<USDCIcon />}
decimals={6}
minDecimals={2}
```

**Make sure to use 6 decimals for USDC!**

When you're done, it should look like this:

```tsx
<ConnectionProvider endpoint={MAINNET_ENDPOINT}>
    <WalletProvider wallets={wallets} autoConnect={connectWallet}>
        <WalletModalProvider>
            <ConfigProvider
                baseURL={baseURL}
                link={link}
                recipient={recipient}
                label={label}
                message={message}
                splToken={MAINNET_USDC_MINT}
                symbol="USDC"
                icon={<USDCIcon />}
                decimals={6}
                minDecimals={2}
                connectWallet={connectWallet}
            >
```

## Using Transaction Requests

[Transaction Requests](https://github.com/solana-labs/solana-pay/blob/master/SPEC.md#specification-transaction-request) are a new feature in Solana Pay.

In the [`client/components/pages/App.tsx`](https://github.com/solana-labs/solana-pay/blob/master/examples/point-of-sale/src/client/components/pages/App.tsx) file, toggle these lines:

```tsx
    // Toggle comments on these lines to use transaction requests instead of transfer requests.
    const link = undefined;
    // const link = useMemo(() => new URL(`${baseURL}/api/`), [baseURL]);
```

When you're done, it should look like this:

```tsx
    // Toggle comments on these lines to use transaction requests instead of transfer requests.
    // const link = undefined;
    const link = useMemo(() => new URL(`${baseURL}/api/`), [baseURL]);
```

The generated QR codes in the app should now use transaction requests. To see what's going on and customize it, check out the [`server/api/index.ts`](https://github.com/solana-labs/solana-pay/blob/master/examples/point-of-sale/src/server/api/index.ts) file.

## Deploying to Vercel

You can deploy this point of sale app to Vercel with a few clicks.

### 1. Fork the project

Fork the Solana Pay repository

### 2. Login to Vercel

Login to Vercel and create a new project

![](./setup/1.New.png)

Import the forked repository from GitHub.

![](./setup/2.Import.png)

> If you're forked repository is not listed, you'll need to adjust your GitHub app permissions. Search for the and select the `Missing Git repository? Adjust GitHub App Permissions` option.

### 3. Configure project

Choose `point-of-sale` as the root directory:

![](./setup/3.Root_directory.png)

Configure the project as follows:

![](./setup/4.Configuration.png)

### Deploy project

Once the deployment finishes, navigate to

```
https://<YOUR DEPLOYMENT URL>?recipient=<YOUR WALLET ADDRESS>&label=Your+Store+Name
```

## License

The Solana Pay Point of Sale app is open source and available under the MIT License. See the [LICENSE](./LICENSE) file for more info.

<!-- Links -->

[1]: https://help.phantom.com/hc/en-us/articles/8071074929043-How-to-Create-a-New-Wallet
[3]: https://solfaucet.com/
