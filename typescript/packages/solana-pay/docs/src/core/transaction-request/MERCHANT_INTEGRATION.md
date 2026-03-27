---
title: Create a transaction request
slug: /core/transaction-request/merchant-integration
---

This section describes how a merchant can integrate Solana Pay transaction requests into their payments flow.

This guide walks through an example of how you can configure a server to respond to a Solana Pay transaction request to initiate a simple SPL token transfer.

A complete example can be found [here][4].

## Requirements

For this example, we'll be building our server using [NextJS API routes][1]. There are no rigid requirements on where your server is deployed and what technologies or languages are used.

---

## 1. Set-up Solana Pay

Install Solana Pay libraries to access the API from your application:

```shell
pnpm add @solana/pay
```

### 1. Create the handler

The handler is the entry point of the API, and "handles" all incoming requests.

```javascript
const index = async (request, response) => {
    // We set up our handler to only respond to `GET` and `POST` requests.
    if (request.method === 'GET') return get(request, response);
    if (request.method === 'POST') return post(request, response);

    throw new Error(`Unexpected method ${request.method}`);
};
```

## 2. The link

A Solana Pay transaction request URL describes an interactive request for any Solana transaction.

```html
solana:<link>
```

A single [link][3] field is required as the pathname. The value must be an absolute HTTPS URL. If the URL contains query parameters, it must be URL-encoded.

```html
solana:https://example.solanapay.com
```

Our server `https://example.solanapay.com` needs to be configured to respond correctly to `GET` and `POST` requests.

## 3. The GET request

The first part of the transaction request spec is for the wallet to make a `GET` request to the specified link.

```javascript
const get = async (request, response) => {
    const label = 'Exiled Apes Academy';
    const icon = 'https://exiledapes.academy/wp-content/uploads/2021/09/X_share.png';

    response.status(200).send({
        label,
        icon,
    });
};
```

The `GET` endpoint should respond with two properties. `label` describes the source of the transaction request. For example, this might be the name of a brand, store, application, or person making the request. `icon` must be an SVG, PNG, or WebP image. The icon and label will be displayed to the user.

## 4. The POST request

The second part of the transaction request spec is the `POST` request.

```typescript
import { address } from '@solana/kit';
import { createMerchantClient } from '@solana/pay';
import { getTransferSolInstruction } from '@solana-program/system';

const MERCHANT_WALLET = address(process.env.MERCHANT_WALLET);

const merchant = createMerchantClient({
    rpcUrl: 'https://api.mainnet-beta.solana.com',
});

const post = async (request, response) => {
    // Account provided in the transaction request body by the wallet.
    const accountField = request.body?.account;
    if (!accountField) throw new Error('missing account');

    const sender = address(accountField);

    // You should always calculate the order total on the server to prevent
    // people from directly manipulating the amount on the client
    const lamports = calculateCheckoutAmountInLamports();

    // Build a SOL transfer instruction using the System Program
    const transferIx = getTransferSolInstruction({
        source: sender,
        destination: MERCHANT_WALLET,
        amount: lamports,
    });

    // Build and serialize the transaction for the wallet to sign
    const base64Transaction = merchant.pay.buildTransaction(sender, [transferIx]);
    const message = 'Thank you for your purchase of ExiledApe #518';

    response.status(200).send({ transaction: base64Transaction, message });
};
```

The wallet will make a `POST` request to the specified link with the user's wallet address as the `account` property of the request body.

The `POST` endpoint should respond with a base64-encoded `transaction`. You can return an optional `message` property to describe the transaction.

### 4.1 The transaction response

The `transaction` that's returned can be -- anything. It doesn't even need to be a payment. For example, it could be a transaction to receive a gift or an invitation from the merchant for scanning a wallet.

<details>
    <summary>Some ideas of what transactions you can do.</summary>

-   Merchants get an atomic bidirectional communication channel with customers. They can mint an NFT or transfer loyalty reward tokens in the transaction.
-   Merchants could potentially see what tokens a user has, accepting and denominating payment in any of them.
-   Merchants can pay for transactions on their user's behalf so they don't need SOL in a wallet.
-   Merchants can return an error from the server to decline to respond with a transaction. This could be used to allow permissioned payments.
-   Payments can be directed to escrow-like programs, enabling things like refunds, chargebacks, and other return mechanisms.
-   DeFi transactions could be bridged to all kinds of web2 / IRL portals.
-   Wallets can retrieve other information, or merchants can pass it to them, like an icon to display, or other fields in the JSON response.
-   It doesn't even need to be a payment. Merchants could send tokens, invitations, gifts to customers that connect a wallet, perhaps one that meets some criteria, such as possessing an NFT.

</details>

For our example, we build a simple native SOL transfer using `getTransferSolInstruction` from `@solana-program/system`. For SPL token transfers, use `getTransferCheckedInstruction` from `@solana-program/token` instead.

## Best Practices

We recommend handling a customer session in a secure environment. Building a secure integration with Solana Pay requires a payment flow as follows:

![](../../images/transaction-request-flow-dark.png)

1. Customer goes to the payment page
2. Merchant frontend (client) sends order information to the backend
3. Merchant backend (server) generates a reference public key and stores it in a database with the expected amount for the shopping cart / pending purchase (unique to each customer's checkout session).
4. Merchant backend redirects the user to the confirmation page with the generated reference public key.
5. The confirmation page redirects to the merchant with the transaction signature.
6. Merchant backend checks that the transaction is valid for the checkout session by validating the transaction with the reference and amount stored in step 3.

<!-- References -->

[1]: https://nextjs.org/docs/api-routes/introduction
[2]: https://github.com/solana-foundation/pay/tree/main/typescript/packages/solana-pay/examples/point-of-sale
[3]: https://github.com/solana-foundation/pay/blob/main/typescript/packages/solana-pay/spec/SPEC.md#link
[4]: https://github.com/solana-foundation/pay
