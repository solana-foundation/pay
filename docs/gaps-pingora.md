# Payment interop gaps & bugs — cross-language audit

During the Pingora gateway rewrite and the cross-language harness hardening we
hit a run of bugs. Many are **protocol / crypto / lifecycle** level, not
Pingora-specific — i.e. any SDK that builds, verifies, or settles payments
(Go, Kotlin, Swift, PHP, Ruby, Python, TypeScript, Rust) can reproduce them.

This file is an **audit checklist**: for each gap, the symptom, the root cause,
how it was fixed in `pay` / `pay-kit` (Rust), and an explicit **AUDIT** action
to check the other-language SDKs against.

Legend for relevance:
- 🌐 **cross-language** — wire/crypto/lifecycle behavior every SDK must get right.
- 🦀 **pay-internal** — specific to the `pay` Pingora data plane; included for the
  lesson, but the fix lives in `pay`, not the SDKs.

---

## 1. 🌐 Transaction decoding: v0 vs legacy (`bincode` trailing-byte trap)

**Symptom.** Server rejected real payments with bogus errors — e.g. MPP charge
failing with *"fee payer must be F82J…"*, session open failing with *"unexpected
end of file"* / a garbage offset. The Rust pay client passed; the canonical JS
client failed.

**Root cause.** `bincode::deserialize::<Transaction>` (the *legacy* type) silently
**ignores trailing bytes**, so a **v0 (versioned) transaction** deserializes into
a *garbage legacy* transaction instead of erroring. The canonical JS/web clients
build **v0** transactions (`createTransactionMessage({version:0})` /
`getBase64EncodedWireTransaction`); the Rust client built legacy. A legacy-first
decoder mis-reads every v0 payload.

**Fix.** Decode straight to `VersionedTransaction`, whose deserializer dispatches
on the version-prefix byte and handles **both** legacy and v0. Done in
`pay-kit` `core/payment_channels::decode_transaction`,
`mpp/server/charge.rs`, and `x402` upto/exact.

**AUDIT (every language).** Any server that decodes a base64/bincode transaction
(charge, channel `open`, exact `Transaction` proof) MUST use the *versioned*
deserializer and accept both legacy and v0 wire formats. A server that assumes
legacy will mis-decode every v0 client and produce confusing downstream errors
(wrong fee payer, bad account indices) rather than a clean parse failure. Test:
feed each server a v0-encoded payment from the canonical TS client.

---

## 2. 🌐 x402 `upto` envelope: `scheme`/`network` live in `accepted`, not the top level

**Symptom.** Rust upto server initially failed canonical envelopes with
*"missing field scheme"*; the **Python** upto server rejected them with
*"invalid payload type: None"* (`envelope.get("scheme")` was `None`).

**Root cause.** Per the canonical x402 v2 spec (`specs/x402-specification-v2.md`
§5.2), `PaymentPayload` is `{ x402Version, accepted, payload }`. **`scheme` and
`network` are required fields of `accepted`** (the chosen `PaymentRequirements`);
there is **no envelope-level `scheme`/`network`**. Servers that read a top-level
`scheme`/`network` get `None`/missing and reject otherwise-valid payments.

**Fix.** Read `scheme` and `network` strictly from `accepted` (Rust: dropped the
envelope-level fields; Python: `accepted.get("scheme")` / `accepted.get("network")`).
No top-level fallback, no defaulting.

**AUDIT (every language, x402 upto + exact servers).** Confirm the server reads
`scheme`/`network` from `accepted.*`, not `envelope.scheme` / `envelope.network`.
Confirm any test that injects a "wrong scheme/network" mutates `accepted.*`
(mutating a non-existent top-level field silently passes). Confirm the client
emits the canonical `{ x402Version, accepted, payload }` shape (no top-level
scheme/network).

---

## 3. 🌐 x402 `accepted` deserialized too strictly

**Symptom.** A canonical-compatible client that echoed an `accepted` object
missing a field the server never reads (e.g. `maxTimeoutSeconds`, `extra.feePayer`)
was rejected during header parsing — before verification even ran.

**Root cause.** `accepted` was parsed as a fully-typed `UptoRequirements`,
requiring every field. The server only needs `network` (and `scheme`) from it.

**Fix.** Keep `accepted` as opaque JSON (`serde_json::Value`); read only the
fields actually used.

**AUDIT.** The echoed `accepted` should be treated as loose/forward-compatible.
Don't hard-fail on fields the server doesn't consume — only validate what you
read. (Languages with strict typed deserialization — Kotlin/Swift/Go structs —
are most at risk here.)

---

## 4. 🌐 x402 `exact`: server blockhash hints break multi-option credential matching

**Symptom.** When the server is configured with an RPC URL, a valid multi-option
`exact` payment failed with *"Credential's accepted does not match any offered
payment option."*

**Root cause.** With `rpc_url` set, the 402 challenge stamps **build hints**
(`extra.recentBlockhash`, `extra.lastValidBlockHeight`, and a top-level
`recentBlockhash`, x402-foundation/x402#2693) into each offered option. The
client echoes them back in `accepted`. But `find_matching_requirement` compared
the echoed `accepted` against options **rebuilt without** the hints → never
equal. A second trap: because a parsed `PaymentRequirements` round-trips its raw
`accepted` verbatim, a **top-level** `recentBlockhash` survived an `extra`-only
strip.

**Fix.** Strip the transient hints (`recentBlockhash` + `lastValidBlockHeight`,
**both** in `extra` and the top-level `recentBlockhash`) from both sides before
comparing — `find_matching_requirement` and the structural backstop.

**AUDIT (servers that embed a recent blockhash in the challenge).** Any field the
server adds to the challenge that the client echoes but the verifier rebuilds
without MUST be normalized/stripped before equality-comparing the echoed
`accepted` to the offered option. Check both nested (`extra.*`) and top-level
copies.

---

## 5. 🌐 x402 `exact`: sponsored fee payer must be account index 0

**Symptom (security).** Greptile/EFe P1. The fee-payer co-sign helper found the
sponsor key *anywhere* in `static_account_keys()` and signed that slot.

**Root cause.** Solana's fee payer is **always account index 0**. Searching the
whole account list means a crafted transaction can put another signer at index 0
and the sponsor later in the required-signer list — the helper signs the wrong
slot, leaving the real fee-payer slot controlled by someone else; broadcast
fails or (worse) settles a transaction whose fee payer isn't the sponsor.

**Fix.** Require `tx.message.static_account_keys().first() == fee_payer_key`,
reject otherwise, then sign `signer_index = 0`.

**AUDIT (every language co-signing sponsored exact txs).** Enforce the sponsor at
index 0; never "find the key anywhere". Same applies to MPP charge fee
sponsorship (verify checks `tx.static_account_keys().first() == feePayerKey`).

---

## 6. 🌐 MPP charge: `externalId` / on-chain memo must match the challenge

**Symptom.** Split charge failed with *"externalId mismatch"*; the on-chain memo
was a random nonce hex instead of the resource.

**Root cause.** The challenge advertised `external_id` (memo = endpoint
`resource`), but the verify path built `ChargeOptions` **without** the same
`external_id`, so the rebuilt expectation didn't match the client's tx. For x402
exact, the memo defaulted to a random nonce when `extra.memo` was absent.

**Fix.** Thread `resource` into both `build_challenge` and verify so `external_id`
(MPP) / `extra.memo` (x402) match; the main-recipient settlement memo = `resource`.

**AUDIT.** If the challenge advertises a memo/`externalId`, the verifier must
reconstruct requirements with the **same** value. Client and server must agree on
the memo source (resource vs nonce). Mismatch = false "verification_failed".

---

## 7. 🌐 MPP session: challenge must advertise pull + clientVoucher

**Symptom.** The payment-channel session opener failed: *"payment-channel session
opener requires a pull-mode session challenge."*

**Root cause.** The canonical `session()` adapter / JS client opens in
**pull + clientVoucher** mode (client signs each voucher, operator only
fee-pays/settles). A push-only challenge can't be opened by that client.

**Fix.** Advertise `modes: [pull]` + `pull_voucher_strategy: client_voucher`;
made pull the default when `modes` is omitted.

**AUDIT (session servers).** The session 402 challenge must advertise pull +
clientVoucher for the canonical opener. Verify the default isn't push-only.

---

## 8. 🌐 Payment-channel settlement: `rent_payer` and required signers

**Symptom.** Settlement failed on-chain with custom program error **`0xA`
(`InvalidChannelRentPayer`)**, and later *"signature verification failed."*

**Root cause.** Two distinct constraints in the payment-channels program
(`CHNLxYvVA28MJP9PrFuDXccuoGXAx7jBacfLEkahyGsX`):
1. `distribute`'s **`rent_payer` must equal the channel's stored rent payer** (the
   advertised operator / channel payer `P`, a **non-signer** account here) — NOT
   the settlement signer `F`. Pinning it to `F` → `0xA`.
2. `settle_and_finalize`'s `merchant` (account 0) is a **required `[signer]`** and
   equals the recipient. Settling without that signature → signature-verification
   failure.

**Fix.** Sign settlement with the recipient/settlement signer `F`; pass the
channel's stored rent payer `P` as the (non-signer) `rent_payer` of `distribute`.
For the channel `open` submit, fee-pay with the channel payer signer (`P`), not
the settlement signer — else *"fee payer does not match operator."*

**AUDIT (every language that settles payment channels).** distribute `rent_payer`
= channel's stored rent payer (operator/channel payer), passed as a **non-signer**;
`settle_and_finalize` must be **signed by the recipient/merchant**; channel
`open` must be fee-paid by the channel payer (operator), distinct from the
settlement signer (the program rejects `payer == payee`).

---

## 9. 🌐 Network slug normalization: `mainnet-beta` → `mainnet`

**Symptom (money, P1).** Real **mainnet** `upto` payments broke: `--mainnet` was
rejected, and the wallet keyed under `"mainnet"` wasn't found.

**Root cause.** The client mapped the CAIP-2 network to an SDK cluster name via
`cluster_for_caip2_network` (returns **`"mainnet-beta"`**) and used it verbatim;
`normalize_network` (which collapses `mainnet-beta` → `mainnet`) never ran. The
network-intent check (exact string compare) and the wallet lookup
(`account_for_network`) both key on the canonical pay slug `"mainnet"`.

**Fix.** Run the mapped cluster through `normalize_network` before the
network-intent check / wallet keying (mirroring the exact path).

**AUDIT (every client).** Normalize SDK/explorer cluster names
(`mainnet-beta`, `solana`, `solana-devnet`, …) to the canonical pay slug
(`mainnet` / `devnet` / `testnet` / `localnet`) **before** any
string-equality network check or wallet-account lookup. `mainnet-beta` is only
ever valid as (a) the literal RPC hostname `api.mainnet-beta.solana.com` or
(b) the Solana Explorer `?cluster=mainnet-beta` query param — never as a stored
or compared pay network slug.

---

## 10. 🌐 Settle-before-serve (exact): attach the `PAYMENT-RESPONSE` receipt on **every** exit path

**Symptom (money, P1, several variants).** For x402 `exact`, the gate settles
on-chain **before** serving (in `x402_exact_verify`, before returning `Forward`).
The client was charged but then received a response with **no `PAYMENT-RESPONSE`
header** — no proof of payment — when:
- the route was respond-mode (no upstream `response_filter` runs),
- upstream prep failed (bad URL / OAuth2 token fetch / body-prep),
- the upstream **TCP/TLS connection** failed (down / unresolvable host),
- the request was refused with a 501 (body-signing auth, see §11).

**Root cause.** The receipt was only attached on the "upstream responded
successfully" path. Settle-before-serve means a charge can complete while the
serve fails for many reasons.

**Fix.** A shared `drain_payment_headers` helper attaches the receipt (and
refunds any `upto` channel) on **all** terminal paths: respond-mode/error
(`finish_inline`), connect/proxy failure (`fail_to_proxy`), and the body-signing
501. The axum adapter already appends receipt headers unconditionally after the
handler.

**AUDIT (every server, especially settle-before-serve).** Enumerate **every**
way a request can terminate after settlement — success, handler error, upstream
unreachable, auth refusal, timeout — and attach the `PAYMENT-RESPONSE` (and MPP
`payment-receipt` / receipt URL) on **each** one. A client that paid must always
get its settlement signature back. This is the gap most likely to be shared by
other-language servers that only set receipt headers on the happy path.

---

## 11. 🌐 Strip payment-credential headers before proxying upstream

**Symptom (security).** The gateway forwarded the x402 v1 `X-PAYMENT` credential
to the third-party upstream, which could log or leak it.

**Root cause.** The strip-list dropped `payment-signature` / `payment-required`
but not `x-payment`.

**Fix.** Added `x-payment` to `STRIP_HEADERS`; the list is shared by both
forwarding paths.

**AUDIT (every proxying server).** Strip **all** payment-credential headers
(`X-PAYMENT`, `PAYMENT-SIGNATURE`, `Authorization` carrying the credential,
`PAYMENT-REQUIRED`, …) before forwarding to the upstream API. Treat the list as
security-sensitive and keep it in one place.

---

## 12. 🌐 Settle-after-serve (`upto`): refund on **every** failure path

**Symptom.** An `upto` channel opened on-chain but the request then failed
(upstream down, prep error) — the deposit could be left stranded in an open
channel.

**Root cause.** `upto` settles **after** serving (debit on 2xx, refund
otherwise). Failure paths that skip the settle step strand the deposit.

**Fix.** Every terminal path settles `upto` with `served_ok = false` (full
refund) when the resource wasn't served (`response_filter`, `finish_inline`,
`fail_to_proxy`, and `logging` as a backstop).

**AUDIT (servers implementing usage/upto-style channels).** On every failure
path after the channel opens, settle `0` (refund) rather than leaving the channel
open. Don't strand client funds.

---

## 13. 🦀 pay-internal (Pingora data plane) — lessons, fixes live in `pay`

These bit the `pay` gateway specifically; noted for completeness and the
transferable lesson.

- **`accepted_schemes()` default regressed sessions.** A new `schemes` YAML field
  defaulting to `[mpp-charge]` silently broke specs that had been accepting
  `intent=session`. Fixed by resolving the default once at spec load
  (`ApiSpec::apply_scheme_defaults`): base `[MppCharge]`, plus `MppSession` when a
  top-level `session:` block exists; explicit `schemes` left untouched.
  *Lesson:* a new "allow-list" field must not silently narrow existing behavior.
- **`unimplemented!()` panicked the worker.** An operator-enabled but
  unimplemented scheme (`X402Upto`) hit `unimplemented!()` and crashed the
  Pingora worker (client saw a dropped connection). Fixed to return a clean
  `501`. *Lesson:* never `panic!`/`unimplemented!` on an operator/attacker-
  reachable request path — return a status.
- **Verified payment on root path mis-routed to the control plane.**
  `is_control_plane("")` was true for the root path, so a paid `Forward` for a
  `path: ""` endpoint went to the internal axum control plane (which re-checked
  the stripped credential and 402'd). Fixed by only applying the control-plane
  shortcut for `Passthrough`, never `Forward`.
- **`axum::serve` exit was silent.** The control-plane task swallowed its exit;
  added `tracing::error!` logging.
- **`to_bytes().unwrap_or_default()` swallowed errors.** Body-read errors
  silently produced an empty response; now logged.
- **HMAC body-signing over a streamed body.** Pingora streams the request body
  unbuffered, so body-digest HMAC/AccessToken upstream auth can't be computed —
  the gate refuses with a clean 501 (`routing_signs_request_body`) rather than
  signing over an empty body. *Lesson (cross-language for streaming proxies):* if
  you can't buffer the body, you can't compute a body signature — fail loudly.

---

## Quick audit matrix

| # | Gap | Layer | Highest-risk SDKs |
|---|-----|-------|-------------------|
| 1 | v0-vs-legacy tx decode | wire | any server decoding txs |
| 2 | scheme/network in `accepted` | wire | all x402 servers |
| 3 | over-strict `accepted` typing | wire | typed-struct langs (Kotlin/Swift/Go) |
| 4 | blockhash-hint option matching | wire | servers embedding recent blockhash |
| 5 | fee payer must be index 0 | crypto | any sponsored-tx co-signer |
| 6 | memo/externalId match | wire | all charge/exact servers |
| 7 | pull+clientVoucher challenge | protocol | session servers |
| 8 | rent_payer / required signers | crypto | any channel settler |
| 9 | mainnet-beta → mainnet | client | all clients |
| 10 | receipt on every exit path | lifecycle | settle-before-serve servers |
| 11 | strip credential headers | security | all proxying servers |
| 12 | refund on every failure | lifecycle | upto/usage servers |
