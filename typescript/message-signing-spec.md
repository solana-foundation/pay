# Solana Pay Message Signing Specification

This spec is currently alpha and subject to change

## Summary
A standard protocol to encode Solana Pay message-signing requests within URLs.

## Motivation
A standard URL protocol for requesting message-signing allows for a better user experience across apps and wallets in the Solana ecosystem.

These URLs may be encoded in QR codes or NFC tags, or sent between users.

Applications should ensure that a signed message is valid before they grant access to objects or events.

Mobile wallets should register to handle the URL scheme to provide a seamless yet secure experience when Solana Pay URLs are encountered in the environment.

By standardizing a simple approach to solving these problems, we ensure basic compatibility of applications and wallets so developers can focus on higher level abstractions.

## Specification: Message Signing

A Solana Pay sign-message request URL describes an interactive request where the parameters in the URL are used by a wallet to make an HTTP request to a remote server to verify ownership of an address.

### Link
```html
solana:<link>
```

A single `link` field is required as the pathname. The value must be a conditionally [URL-encoded](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/encodeURIComponent) absolute HTTPS URL.

If the URL contains query parameters, it must be URL-encoded. Protocol query parameters may be added to this specification. URL-encoding the value prevents conflicting with protocol parameters.

If the URL does not contain query parameters, it should not be URL-encoded. This produces a shorter URL and a less dense QR code.

In either case, the wallet must [URL-decode](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/decodeURIComponent) the value. This has no effect if the value isn't URL-encoded. If the decoded value is not an absolute HTTPS URL, the wallet must reject it as **malformed**.

#### GET Request

The wallet should make an HTTP `GET` JSON request to the URL. The request should not identify the wallet or the user.

The wallet should make the request with an [Accept-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Accept-Encoding), and the application should respond with a [Content-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Encoding) for HTTP compression.

The wallet should display the domain of the URL as the request is being made.

#### GET Response

The wallet must handle HTTP [client error](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#client_error_responses), [server error](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#server_error_responses), and [redirect responses](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#redirection_messages). The application must respond with these, or with an HTTP `OK` JSON response with a body of
```json
{"label":"<label>","icon":"<icon>"}
```

The `<label>` value must be a UTF-8 string that describes the source of the sign-message request. For example, this might be the name of a brand, store, application, or person making the request.

The `<icon>` value must be an absolute HTTP or HTTPS URL of an icon image. The file must be an SVG, PNG, or WebP image, or the wallet must reject it as **malformed**.

The wallet should not cache the response except as instructed by [HTTP caching](https://developer.mozilla.org/en-US/docs/Web/HTTP/Caching#controlling_caching) response headers.

The wallet should display the label and render the icon image to user.

#### POST Request

The wallet must make an HTTP `POST` JSON request to the URL with a body of
```json
{"account":"<account>"}
```

The `<account>` value must be the base58-encoded public key of the account that will sign the message.

The wallet should make the request with an [Accept-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Accept-Encoding), and the application should respond with a [Content-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Encoding) for HTTP compression.

The wallet should display the domain of the URL as the request is being made. If a `GET` request was made, the wallet should also display the label and render the icon image from the response.

#### POST Response

The wallet must handle HTTP [client](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#client_error_responses) and [server](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#server_error_responses) errors in accordance with the [error handling](#error-handling) specification. [Redirect responses](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#redirection_messages) must be handled appropriately. The application must respond with these, or with an HTTP `OK` JSON response with a body of
```json
{"data":"<data>","state":"<state>"}
```

The `<data>` value must be arbitrary bytes, encoded as a base64 string. The wallet must base-64 decode and sign the `data` value with the private key that corresponds to the `account` in the request and send `data` and the resulting signature back to the server in the proceeding [PUT request](https://github.com/bedrock-foundation/solana-pay/edit/master/SPEC.md#put-request).

The `<state>` value must be a [UTF-8](https://developer.mozilla.org/en-US/docs/Glossary/UTF-8) string value, and must contain a [MAC](https://en.wikipedia.org/wiki/Message_authentication_code). The wallet will pass this value back to the server in the [PUT request](https://github.com/bedrock-foundation/solana-pay/edit/master/SPEC.md#put-request) in order to verify that the contents of the `<data>` field were not modified.


The application may also include an optional `message` field in the response body:
```json
{"message":"<message>","data":"<data>","state":"<state>"}
```

The `<message>` value must be a UTF-8 encoded string that describes the nature of the sign-message response.

For example, this might be the name of the application with which the user is interacting or context about how the sign-message request is being used. The wallet must display at least the first 80 characters of the message field to the user if it is included in the response.

The application may also include an optional `redirect` field in the response body:
```json
{"redirect":"<redirect>", "message":"<message>","data":"<data>","state":"<state>"}
```

The `redirect` field must be an absolute HTTPS or solana URL.

If it is a HTTPS URL then the wallet should display the decoded value to the user. 

The wallet and application should allow additional fields in the request body and response body, which may be added by future specification.

#### PUT Request

The PUT request is used to send the results of signing the message back to the server. The wallet must make an HTTP `PUT` JSON request to the URL with a body of
```json
{"account":"<account>","data":"<data>","state":"<state>","signature":"<signature>"}
```

The `<account>` value must be the base58-encoded public key of the account that signed the message.

The `<data>` value must be the unmodified, base64-encoded `<data>` value from the response of the preceding POST request.

The `<state>` value must be the unmodified `<state>` value from the response of the preceding POST request.

The `<signature>` value is the base64-encoded signature from signing the `<data>` field with the users private key.

The wallet should make the request with an [Accept-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Accept-Encoding), and the application should respond with a [Content-Encoding header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Encoding) for HTTP compression.

The wallet should display the domain of the URL as the request is being made. If a `GET` request was made, the wallet should also display the label and render the icon image from the response.

#### PUT Response

The wallet must handle HTTP [client](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#client_error_responses) and [server](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#server_error_responses) errors in accordance with the [error handling](#error-handling) specification. [Redirect responses](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#redirection_messages) must be handled appropriately. The application must respond with these, or with an HTTP `OK` response. An HTTP `OK` response indicates that signature verification was successful.

If signature verification was successful and there was a `redirect` field in the POST response, then the decoded redirect URL should be followed. If the redirect is a HTTPS URL then the wallet should open the URL using any available browser. This may be a browser included in the wallet. If it is a `solana:` URL then the wallet should treat it as a new Solana Pay request.

The wallet and application should allow additional fields in the request body and response body, which may be added by future specification.

#### Error Handling
If the application responds with an HTTP [client](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#client_error_responses) or [server](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#server_error_responses) error in response to the POST or PUT operations, the wallet must consider the entire message-signing request as failed. 

Client and server errors may optionally be accompanied by a JSON body containing a UTF-8 string `message` field describing the nature of the error:
```json
{"message":"<message>"}
```

The wallet must display at least the first 80 characters of the `message` field to the user if it is included in the response.

### Sign-message Request Example

##### URL describing a sign-message request.
```
solana:https://example.com/solana-pay/sign-message
```

##### URL describing a sign-message request with query parameters.
```
solana:https%3A%2F%2Fexample.com%2Fsolana-pay%2Fsign-message%3Fid%3D678910
```

##### GET Request
```
GET /solana-pay/sign-message?id=678910 HTTP/1.1
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
POST /solana-pay/sign-message?id=678910 HTTP/1.1
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

{"message":"Sign the message to login","data":"SIGN_THIS_MESSAGE","state":"eyJhbGciOiJIUzI1NiJ9.U0lHTl9USElTX01FU1NBR0U.KcZ1FnrT1ImAL-7LbALfZOx9F4I4LMuEE8_bg5Zmec4"}
```

##### PUT Request
```
POST /solana-pay/sign-message?id=678910 HTTP/1.1
Host: example.com
Connection: close
Accept: application/json
Accept-Encoding: br, gzip, deflate
Content-Type: application/json
Content-Length: 57

{"account":"mvines9iiHiQTysrwkJjGf2gb9Ex9jXJX8ns3qwf2kN","data":"SIGN_THIS_MESSAGE","state":"eyJhbGciOiJIUzI1NiJ9.U0lHTl9USElTX01FU1NBR0U.KcZ1FnrT1ImAL-7LbALfZOx9F4I4LMuEE8_bg5Zmec4","signature":"3ApozYFyp2ZxWuGvJS7Q1oV8M3YsLMV3WmwbjGCgktqXfdevjCZ92vA4F9V7Xj7KrN7JTtYStBSBeWnNN7vyHkg5"}
```

##### PUT Response
```
HTTP/1.1 200 OK
Connection: close
Content-Type: application/json
Content-Length: 298
Content-Encoding: gzip

{}
```