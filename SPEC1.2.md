# Solana Pay Specification

This spec is subject to change

## Summary
A standard protocol to encode Solana transaction requests within URLs to enable payments and other use cases.

This standard draws inspiration from [BIP 21](https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki) and [EIP 681](https://github.com/ethereum/EIPs/blob/master/EIPS/eip-681.md).

## Motivation
A standard URL protocol for requesting native SOL transfers, SPL Token transfers, and Solana transactions allows for a better user experience across apps and wallets in the Solana ecosystem.

These URLs may be encoded in QR codes or NFC tags, or sent between users and applications to request payment and compose transactions.

Applications should ensure that a transaction has been confirmed and is valid before they release goods or services being sold, or grant access to objects or events.

Mobile wallets should register to handle the URL scheme to provide a seamless yet secure experience when Solana Pay URLs are encountered in the environment.

By standardizing a simple approach to solving these problems, we ensure basic compatibility of applications and wallets so developers can focus on higher level abstractions.

## Specification: Transfer Request

A Solana Pay transfer request URL describes a non-interactive request for a SOL or SPL Token transfer.
```html
solana:<recipient>
      ?amount=<amount>
      &spl-token=<spl-token>
      &token-program=<token-program>
      &payment-options=<payment-options>
      &reference=<reference>
      &label=<label>
      &message=<message>
      &memo=<memo>
      &fee-payer=<fee-payer>
      &fee-payer-server=<fee-payer-server>
      &fee-payer-fee=<fee-payer-fee>
      &redirect=<redirect>
```

The request is non-interactive because the parameters in the URL are used by a wallet to directly compose a transaction.

### Recipient
A single `recipient` field is required as the pathname. The value must be the base58-encoded public key of a native SOL account. Associated token accounts must not be used.

Instead, to request an SPL Token transfer, the `spl-token` field must be used to specify an SPL Token mint, from which the associated token address of the recipient must be derived.

### Amount
A single `amount` field is allowed as an optional query parameter. The value must be a non-negative integer or decimal number of "user" units. For SOL, that's SOL and not lamports. For tokens, use [`uiAmountString` and not `amount`](https://docs.solana.com/developing/clients/jsonrpc-api#token-balances-structure).

`0` is a valid value. If the value is a decimal number less than `1`, it must have a leading `0` before the `.`. Scientific notation is prohibited.

If a value is not provided, the wallet must prompt the user for the amount. If the number of decimal places exceed what's supported for SOL (9) or the SPL Token (mint specific), the wallet must reject the URL as **malformed**.

### SPL Token
A single `spl-token` field is allowed as an optional query parameter. The value must be the base58-encoded public key of an SPL Token mint account.

If the field is provided, the [Associated Token Account](https://spl.solana.com/associated-token-account) convention must be used, and the wallet must include a `TokenProgram.Transfer` or `TokenProgram.TransferChecked` instruction as the last instruction of the transaction.

If the field is not provided, the URL describes a native SOL transfer, and the wallet must include a `SystemProgram.Transfer` instruction as the last instruction of the transaction instead.

The wallet must derive the ATA address from the `recipient` and `spl-token` fields. Transfers to auxiliary token accounts are not supported.

### Token Program
A single `token-program` field is allowed as an optional query parameter. The value must be the base58-encoded public key of a token program.

- If `spl-token` is provided, the wallet must use the specified `token-program` to derive accounts and to include the corresponding transfer instruction (e.g. Token-2022).
- If `token-program` is not provided, wallets and libraries should attempt to determine the correct program automatically from on-chain mint data. If auto-detection fails, default to the legacy SPL Token Program (`TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`) for backwards compatibility.
- Compressed tokens typically require additional proofs and must be handled via the Transaction Request flow. Wallets may reject a non-interactive transfer request that implies a compressed token transfer as **unsupported**.

Examples:
- Token-2022: `token-program=TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`

### Reference
Multiple `reference` fields are allowed as optional query parameters. The values must be base58-encoded 32 byte arrays. These may or may not be public keys, on or off the curve, and may or may not correspond with accounts on Solana.

If the values are provided, the wallet must include them in the order provided as read-only, non-signer keys to the `SystemProgram.Transfer` or `TokenProgram.Transfer`/`TokenProgram.TransferChecked` instruction in the payment transaction. The values may or may not be unique to the payment request, and may or may not correspond to an account on Solana.

Because Solana validators index transactions by these account keys, `reference` values can be used as client IDs (IDs usable before knowing the eventual payment transaction). The [`getSignaturesForAddress`](https://docs.solana.com/developing/clients/jsonrpc-api#getsignaturesforaddress) RPC method can be used locate transactions this way.

### Payment Options
A single `payment-options` field is allowed as an optional query parameter. The value must be a semicolon-separated list of payment choices. Each choice must be a comma-separated pair of `<currency>,<amount>` where:

- `<currency>` is either the string `sol` (case-insensitive) to indicate native SOL, or the base58-encoded public key of an SPL Token mint account.
- `<amount>` is a non-negative integer or decimal number in user units for the corresponding currency. Scientific notation is prohibited. If the number of decimal places exceeds what's supported for the selected currency, the wallet must reject the URL as **malformed**.

Examples:
- `payment-options=sol,1` (1 SOL)
- `payment-options=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v,0.01` (0.01 USDC)
- `payment-options=sol,1;EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v,0.01` (user may choose)
 - With a top-level `spl-token` and `amount`, it's required to mirror the same option here for compatibility.

Rules:
- If `spl-token` is provided and `payment-options` is also present, wallets may offer both the top-level option (defined by `spl-token` and the top-level `amount`, if present) and the parsed `payment-options` list. The top-level option should be treated as the default selection for backwards compatibility. It is required to include an equivalent entry in `payment-options` for the top-level choice.
- If `payment-options` is present and `spl-token` is not provided, wallets that support this parameter should prompt the user to choose a payment option and compose the transfer accordingly.
- When `payment-options` is present and `spl-token` is not provided, wallets should ignore the top-level `amount` and use the amount from the selected payment option. When `spl-token` is provided, the top-level `amount` applies to the top-level selection; amounts in `payment-options` apply when the user selects an entry from that list.
- Wallets that do not support `payment-options` must ignore it and proceed as if it were absent (defaulting to SOL unless `spl-token` is present).

### Label
A single `label` field is allowed as an optional query parameter. The value must be a [URL-encoded](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/encodeURIComponent) UTF-8 string that describes the source of the transfer request.

For example, this might be the name of a brand, store, application, or person making the request. The wallet should [URL-decode](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/decodeURIComponent) the value and display the decoded value to the user.

### Message
A single `message` field is allowed as an optional query parameter. The value must be a [URL-encoded](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/encodeURIComponent) UTF-8 string that describes the nature of the transfer request.

For example, this might be the name of an item being purchased, an order ID, or a thank you note. The wallet should [URL-decode](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/decodeURIComponent) the value and display the decoded value to the user.

### Memo
A single `memo` field is allowed as an optional query parameter. The value must be a [URL-encoded](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/encodeURIComponent) UTF-8 string that must be included in an [SPL Memo](https://spl.solana.com/memo) instruction in the payment transaction.

The wallet must [URL-decode](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/decodeURIComponent) the value and should display the decoded value to the user. The memo will be recorded by validators and should not include private or sensitive information.

If the field is provided, the wallet must include a `MemoProgram` instruction as the second to last instruction of the transaction, immediately before the SOL or SPL Token transfer instruction, to avoid ambiguity with other instructions in the transaction.

### Fee Payer
A single `fee-payer` field is allowed as an optional query parameter. The value must be the base58-encoded public key to be used as the transaction fee payer.

Rules:
- If provided, wallets that support fee relaying should set the transaction `feePayer` to the specified account and must not attempt to sign as that account.
- Wallets may submit the partially signed transaction to a configured relayer service (e.g., a Kora server) to finalize and broadcast. Relayer configuration and discovery are wallet-defined and out of scope for this specification.
- If fee relaying is unsupported or unavailable, wallets should either ignore `fee-payer` and proceed with the sender as the fee payer, or surface a structured error as defined in the Error Handling section (`SP_FEE_PAYER_UNAVAILABLE`). Implementations should prefer not breaking payment flows; merchants relying on relayers should also offer a fallback.

### Fee Payer Server
A single `fee-payer-server` field is allowed as an optional query parameter. If this option is provided, then the `fee-payer` field is required. The value must be an absolute HTTPS URL designating the relayer service endpoint that will accept and process the fee-payer flow for this transaction. This relayer should adhere to the sRFC-34 specification like Kora.

Rules:
- When `fee-payer` is present, wallets that support relaying should use `fee-payer-server` to direct relay submission to the specified service.
- If `fee-payer` is present without `fee-payer-server`, wallets may use their default relayer configuration.
- If `fee-payer-server` is present without `fee-payer`, wallets must ignore it.
- Best practice: keep `fee-payer` as a pure public key and specify the service endpoint via `fee-payer-server`.

### Fee Payer Fee
A single `fee-payer-fee` field is allowed as an optional query parameter. If this option is provided, then the `fee-payer` and `fee-payer-server` fields are required. The value must follow the same semicolon-separated CSV-like format as `payment-options`:

`fee-payer-fee=<currency>,<amount>[;...]`

Where each entry specifies the currency and amount to be paid to the relayer for facilitating the transaction. The currency rules match `payment-options` (`sol` or SPL mint address). Amounts are user units and must comply with currency precision.

Rules:
- If present, wallets should include this additional payment to the fee payer service as part of the composed flow. The mechanism to pay the relayer may require switching to the Transaction Request flow or composing multiple transfers; concrete implementation is wallet-defined.
- If multiple entries are provided, wallets should select the entry matching the chosen payment currency when possible; otherwise wallets may pick any supported entry.
- If unsupported, wallets may ignore `fee-payer-fee` and proceed.

### Redirect
A single `redirect` field is allowed as an optional query parameter. The value must be a [URL-encoded](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/encodeURIComponent) absolute HTTPS or `solana:` URL.

The wallet must [URL-decode](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/decodeURIComponent) the value. If it is a HTTPS URL then the wallet should display the decoded value to the user. 

Redirect URLs should only be followed if the transaction is successful. A transaction should be considered successful if the user approves it and the broadcast transaction has a Confirmed or Finalized status. If the redirect is a HTTPS URL then the wallet should open the URL using any browser. This may be a browser included in the wallet. If it is a `solana:` URL then the wallet should treat it as a new Solana Pay request.

### Examples

##### URL describing a transfer request for 1 SOL.
```
solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN?amount=1&label=Michael&message=Thanks%20for%20all%20the%20fish&memo=OrderId12345
```

##### URL describing a transfer request for 0.01 USDC.
```
solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN?amount=0.01&spl-token=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
```

##### URL describing a transfer request for SOL. The user must be prompted for the amount.
```
solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN?label=Michael
```

##### URL describing a transfer request with payment options; wallet chooses
```
solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN?payment-options=sol,1;EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v,0.01
```

##### URL describing a Token-2022 USDC transfer with a relayer fee payer
```
solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN?amount=0.01&spl-token=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&token-program=TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb&fee-payer=FeePayr1111111111111111111111111111111111111
```

##### URL describing a transfer with fee payer server and relayer fee in an SPL token
```
solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN?payment-options=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v,1&fee-payer=FeePayr1111111111111111111111111111111111111&fee-payer-server=https%3A%2F%2Fkora.example.com%2Frelay&fee-payer-fee=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v,0.001
```

##### URL describing a transfer request for 1 SOL with a redirect
```
solana:mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN?amount=1&label=Michael&message=Thanks%20for%20all%20the%20fish&memo=OrderId12345&redirect=https%3A%2F%2Fexample.com
```

## Specification: Transaction Request

A Solana Pay transaction request URL describes an interactive request for any Solana transaction.
```html
solana:<link>
```

The request is interactive because the parameters in the URL are used by a wallet to make an HTTP request to compose a transaction.

### Link
A single `link` field is required as the pathname. The value must be a conditionally [URL-encoded](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/encodeURIComponent) absolute HTTPS URL.

If the URL contains query parameters, it must be URL-encoded. Protocol query parameters may be added to this specification. URL-encoding the value prevents conflicting with protocol parameters.

If the URL does not contain query parameters, it should not be URL-encoded. This produces a shorter URL and a less dense QR code.

In either case, the wallet must [URL-decode](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/decodeURIComponent) the value. This has no effect if the value isn't URL-encoded. If the decoded value is not an absolute HTTPS URL, the wallet must reject it as **malformed**.

#### GET Request

The wallet should make an HTTP `GET` JSON request to the URL. The request should not identify the wallet or the user.

The wallet should make the request with an [Accept-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Accept-Encoding), and 
should respond with a [Content-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Encoding) for HTTP compression.

The wallet should display the domain of the URL as the request is being made.

#### GET Response

The wallet must handle HTTP [client error](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#client_error_responses), [server error](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#server_error_responses), and [redirect responses](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#redirection_messages). The application must respond with these, or with an HTTP `OK` JSON response with a body of
```json
{"label":"<label>","icon":"<icon>"}
```

The `<label>` value must be a UTF-8 string that describes the source of the transaction request. For example, this might be the name of a brand, store, application, or person making the request.

The `<icon>` value must be an absolute HTTP or HTTPS URL of an icon image. The file must be an SVG, PNG, or WebP image, or the wallet must reject it as **malformed**.

The wallet should not cache the response except as instructed by [HTTP caching](https://developer.mozilla.org/en-US/docs/Web/HTTP/Caching#controlling_caching) response headers.

The wallet should display the label and render the icon image to user.

Optional fields may be included to advertise payment choices:
```json
{"label":"<label>","icon":"<icon>","paymentOptions":[{"currency":"sol","amount":"1"},{"currency":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","amount":"0.01"}]}
```
Where each `currency` value is either `sol` (case-insensitive) or a base58 SPL Token mint address. Applications may additionally include display metadata such as `symbol`, `decimals`, and a fixed `amount` per option if pricing is static.

#### POST Request

The wallet must make an HTTP `POST` JSON request to the URL with a body of
```json
{"account":"<account>"}
```

The `<account>` value must be the base58-encoded public key of an account that may sign the transaction.

The wallet should make the request with an [Accept-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Accept-Encoding), and the application should respond with a [Content-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Encoding) for HTTP compression.

The wallet should display the domain of the URL as the request is being made. If a `GET` request was made, the wallet should also display the label and render the icon image from the response.

If the `GET` response included `paymentOptions`, the wallet may include optional fields to convey the user's selection and amount:
```json
{"account":"<account>","currency":"sol","amount":"<amount>"}
```
`currency` must be either `sol` or a base58 SPL Token mint address. If `amount` is provided, it must be a non-negative integer or decimal number of user units for the selected currency. Applications that do not support these fields must ignore them.

#### POST Response

The wallet must handle HTTP [client](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#client_error_responses) and [server](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#server_error_responses) errors in accordance with the [error handling](#error-handling) specification. [Redirect responses](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#redirection_messages) must be handled appropriately. The application must respond with these, or with an HTTP `OK` JSON response with a body of
```json
{"transaction":"<transaction>"}
```

The `<transaction>` value must be a base64-encoded [serialized transaction](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#serialize). The wallet must base64-decode the transaction and [deserialize it](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#from).

The application may respond with a partially or fully signed transaction. The wallet must validate the transaction as **untrusted**.

If the transaction [`signatures`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#signatures) are empty:
  - The application should set the [`feePayer`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#feePayer) to the `account` in the request, or the zero value (`new PublicKey(0)` or `new PublicKey("11111111111111111111111111111111")`).
  - The application should set the [`recentBlockhash`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#recentBlockhash) to the [latest blockhash](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Connection.html#getLatestBlockhash), or the zero value (`new PublicKey(0).toBase58()` or `"11111111111111111111111111111111"`).
  - The wallet must ignore the [`feePayer`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#feePayer) in the transaction and set the `feePayer` to the `account` in the request.
  - The wallet must ignore the [`recentBlockhash`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#recentBlockhash) in the transaction and set the `recentBlockhash` to the [latest blockhash](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Connection.html#getLatestBlockhash).

If the transaction [`signatures`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#signatures) are nonempty:
  - The application must set the [`feePayer`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#feePayer) to the [public key of the first signature](https://solana-foundation.github.io/solana-web3.js/v1.x/modules.html#SignaturePubkeyPair).
  - The application must set the [`recentBlockhash`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#recentBlockhash) to the [latest blockhash](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Connection.html#getLatestBlockhash).
  - The application must serialize and deserialize the transaction before signing it. This ensures consistent ordering of the account keys, as a workaround for [this issue](https://github.com/solana-labs/solana/issues/21722).
  - The wallet must not set the  [`feePayer`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#feePayer) and [`recentBlockhash`](https://solana-foundation.github.io/solana-web3.js/v1.x/classes/Transaction.html#recentBlockhash).
  - The wallet must verify the signatures, and if any are invalid, the wallet must reject the transaction as **malformed**.

The wallet must only sign the transaction with the `account` in the request, and must do so only if a signature for the `account` in the request is expected.

If any signature except a signature for the `account` in the request is expected, the wallet must reject the transaction as **malicious**.

The application may also include an optional `message` field in the response body:
```json
{"message":"<message>","transaction":"<transaction>"}
```

The `<message>` value must be a UTF-8 string that describes the nature of the transaction response. The wallet must display at least the first 80 characters of the `message` field to the user if it is included in the response.

For example, this might be the name of an item being purchased, a discount applied to the purchase, or a thank you note. The wallet should display the value to the user.

The application may also include an optional `redirect` field in the response body:

```json
{"redirect":"<redirect>","transaction":"<transaction>"}
```

The `redirect` field must be an absolute HTTPS or `solana:` URL.

If it is a HTTPS URL then the wallet should display the decoded value to the user. 

Redirect URLs should only be followed if the transaction is successful. A transaction should be considered successful if the user approves it and the broadcast transaction has a Confirmed or Finalized [Commitment Status](https://docs.solana.com/cluster/commitments). If the redirect is a HTTPS URL then the wallet should open the URL using any browser. This may be a browser included in the wallet. If it is a `solana:` URL then the wallet should treat it as a new Solana Pay request.

The wallet and application should allow additional fields in the request body and response body, which may be added by future specification.

#### Error Handling
If the application responds with an HTTP [client](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#client_error_responses) or [server](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#server_error_responses) error in response to the POST or PUT operations, the wallet must consider the entire transaction request as failed.

Errors should include a structured JSON body with a machine-readable `code` and a human-readable `message`:
```json
{"code":"SP_INVALID_REQUEST","message":"<message>","details":{},"retryAfterMs":0}
```

Fields:
- `code`: A string constant identifying the error. Wallets must use this for programmatic handling when present. Unknown codes must be treated as generic errors.
- `message`: A UTF-8 string describing the error. Wallets must display at least the first 80 characters.
- `details`: Optional object carrying machine-readable context (e.g., offending parameter).
- `retryAfterMs`: Optional number indicating how long the client should wait before retrying.

Standard error codes and recommended HTTP statuses:
- `SP_UNSUPPORTED_VERSION` (400)
- `SP_INVALID_REQUEST` (400)
- `SP_MALFORMED_URL` (400)
- `SP_UNSUPPORTED_PARAMETER` (400)
- `SP_AMOUNT_TOO_PRECISE` (400)
- `SP_AMOUNT_OUT_OF_RANGE` (400)
- `SP_UNAUTHORIZED_ACCOUNT` (403)
- `SP_RESOURCE_NOT_FOUND` (404)
- `SP_ALREADY_PAID` (409)
- `SP_UNSUPPORTED_CURRENCY` (409)
- `SP_PRICE_EXPIRED` (409)
- `SP_RATE_LIMITED` (429)
- `SP_SERVER_ERROR` (500)
- `SP_MAINTENANCE` (503)
- `SP_FEE_PAYER_UNAVAILABLE` (503)

For backwards compatibility, servers may return only a `message` field; wallets must continue to display such messages.

### Example

##### URL describing a transaction request.
```
solana:https://example.com/solana-pay
```

##### URL describing a transaction request with query parameters.
```
solana:https%3A%2F%2Fexample.com%2Fsolana-pay%3Forder%3D12345
```

##### GET Request
```
GET /solana-pay?order=12345 HTTP/1.1
Host: example.com
Connection: close
Accept: application/json
Accept-Encoding: br, gzip, deflate
```

##### GET Response
```
HTTP/1.1 200 OK
Connection: close
Content-Type: application/json
Content-Length: 62
Content-Encoding: gzip

{"label":"Michael Vines","icon":"https://example.com/icon.svg"}
```

##### POST Request
```
POST /solana-pay?order=12345 HTTP/1.1
Host: example.com
Connection: close
Accept: application/json
Accept-Encoding: br, gzip, deflate
Content-Type: application/json
Content-Length: 57

{"account":"mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN"}
```

##### POST Response
```
HTTP/1.1 200 OK
Connection: close
Content-Type: application/json
Content-Length: 298
Content-Encoding: gzip

{"message":"Thanks for all the fish","transaction":"AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAECC4JMKqNplIXybGb/GhK1ofdVWeuEjXnQor7gi0Y2hMcAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQECAAAMAgAAAAAAAAAAAAAA",
"redirect": "https://example.com"}
```

##### Example GET Response with payment options
```
HTTP/1.1 200 OK
Connection: close
Content-Type: application/json
Content-Length: 128
Content-Encoding: gzip

{"label":"Example Store","icon":"https://example.com/icon.svg","paymentOptions":[{"currency":"sol","amount":"1"},{"currency":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","amount":"250.01"}]}
```

##### Example structured error response
```
HTTP/1.1 409 Conflict
Connection: close
Content-Type: application/json
Content-Length: 86
Content-Encoding: gzip

{"code":"SP_UNSUPPORTED_CURRENCY","message":"Currency not supported","details":{"currency":"RANDOMM1n7xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}
```

## Extensions

Additional formats and fields may be incorporated into this specification to enable new use cases while ensuring compatibility with apps and wallets.

Please open a Github issue to propose changes to the specification in order to solicit feedback from application and wallet developers.

[An actual example of such a proposal.](https://github.com/solana-labs/solana-pay/issues/26)

## Backwards Compatibility

This version introduces optional fields and structured error codes in a backwards-compatible manner:
- Unknown query parameters (`payment-options`, `fee-payer`, `fee-payer-server`, `fee-payer-fee`, `token-program`) must be ignored by wallets and libraries that do not recognize them.
- When `token-program` is omitted, implementations should attempt to infer the correct program; default to the legacy Token Program if unsure.
- Transaction Request servers may continue to return only a `message` on error; clients should display it. When `code` is present, clients should prefer it for programmatic handling.
- Existing integrations that use `spl-token` or SOL-only transfers continue to function unchanged.

## What's new in v1.2 (vs v1.1)

- New parameter: `payment-options=<currency,amount>[;...]` to advertise multiple fixed-price choices; additive with top-level `spl-token`/`amount`.
- New parameter: `fee-payer=<pubkey>` for transfer requests to enable relayer-based fee payment.
- New parameter: `fee-payer-server=<https-url>` to direct wallets to a specific relayer endpoint.
- New parameter: `fee-payer-fee=<currency,amount>[;...]` to request a relayer fee in specific currency/amount(s).
- New parameter: `token-program=<programId>` (auto-detect if omitted; supports Token-2022).
- Structured Transaction Request errors `{ code, message, details?, retryAfterMs? }` with standard `SP2_*` codes.
- Transaction Request GET may advertise `paymentOptions`; POST may include `{ currency, amount }` to reflect the user's choice.

## Migration notes (from v1.1)

- Existing URLs without new params behave identically to v1.1.
- If using top-level `spl-token` and `amount` while also providing `payment-options`, mirror the same choice inside `payment-options` (required for consistency and better wallet UX).
- Wallets that ignore unknown params will still process SOL/`spl-token` transfers as in v1.1.
- Servers can continue returning only `message` on error; clients should display it. Prefer handling by `code` when present.
