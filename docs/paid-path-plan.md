# Paid-request path: optimize, secure, harden (plan)

Scope: the server (pingora `Http402Gate`) receiving a payment credential →
verify → settle on-chain → serve upstream → receipt/refund. This is the
**paid** path (not the 402 challenge, already ~69ms after the blockhash cache).

## 1. Current path & per-request on-chain budget

`request_filter` → `PaymentGate::evaluate` (off-chain credential verify, rebuilds
expected challenge from our own price+splits) → on-chain settle → `Forward` →
upstream → `response_filter` (receipt; upto settle) → `logging`/`fail_to_proxy`
(refund/recover).

On-chain RPC per scheme (verify itself is off-chain crypto):

| Scheme | simulate | send (preflight) | confirm (poll) | when |
|---|---|---|---|---|
| MPP charge | explicit `charge.rs:1089` | `send_transaction` `1172` (preflights) | `1189` ≤30×200ms | before serve |
| x402 exact | explicit `exact.rs:669` | `send_and_confirm` `677` | in send_and_confirm | before serve |
| x402 upto open | — | `send_and_confirm` `upto.rs:459` | in send_and_confirm | before serve |
| x402 upto settle | — | `send_and_confirm` `upto.rs:609` | in send_and_confirm | **blocks response** (`http402.rs:406`) |

Dominant cost = the **confirm poll** (`confirmed` commitment). upto pays it
**twice** (open + settle), both synchronous.

## 2. Plan (phased)

### P0 — quick wins, low risk

**P0.1 Condense simulate+send → send-with-preflight (MPP charge, x402 exact).**
Drop the explicit `simulate_transaction`; rely on `send`'s preflight (which
already simulates on the node). On preflight failure the send error carries the
simulation error+logs, so diagnostics survive. Preserve the RPC-lag retry loop
(`SIMULATION_MAX_ATTEMPTS`) by retrying the send-with-preflight on a
lag-shaped error. Saves 1 RPC round-trip/request.
- Files: `pay-kit` `mpp/server/charge.rs` `broadcast_pull`, `x402/server/exact.rs` `settle_exact`.
- Risk: low. Keep `skip_preflight=false`; move balance-diagnostics into the final-failure path.

**P0.2 Defer the upto settle off the response path.**
Split `settle_actual` into `sign_settlement` (returns signed tx + signature, no
broadcast) and `broadcast_settlement` (send+confirm). In `response_filter`: sign,
put the signature in `PAYMENT-RESPONSE`, write the response, then broadcast async.
Client funds are already locked by the confirmed `open`, so deferring settle is
operator-risk, not client-loss.
- Files: `pay-kit` `x402/server/upto.rs` (split `settle_actual`); `pay`
  `crates/proxy/src/http402.rs` `response_filter` + a broadcaster.
- Durability: P0 = fire-and-forget + existing channel-store sweep backstop;
  upgraded in P1.2.
- Removes one full confirm from the client path.

### P1 — confirm off the hot path + multi-instance correctness

**P1.1 Optimistic serve for the pre-serve gate (the big latency win).**
Today charge/exact/upto-open confirm before serving (don't serve against an
unconfirmed payment). Options, in order of preference:
- (a) **Persistent upto channel** (session-like): open once (confirmed), then
  many requests settle against the same channel — amortizes the open confirm to
  ~zero per request. Best fit for high-RPS clients (the gemini case).
- (b) **Serve after send-accepted** (RPC accepted the broadcast ⇒ tx passed
  preflight and is in the mempool), confirm async + reconcile. Risk: rare
  serve-then-not-landed; bound by preflight already passing + reconcile/clawback.
- (c) Gate on a lower commitment (`processed`) + async `confirmed`.
- Requires P1.2 (durable replay) before relaxing the gate.

**P1.2 Durable replay + channel store (multi-instance).**
Today MPP replay = in-memory `MemoryStore` (pay never sets a durable store) and
`X402Upto.in_flight` = per-instance `Mutex<HashSet>`. With `max-instances=10`,
a credential/channel consumed on instance A is invisible to B ⇒ cross-instance
replay / double-serve window. Inject a shared store (Redis/Firestore).
- Files: `pay` `start.rs` Mpp/X402 construction (`store:`/channel store);
  `pay-kit` `Store`/`ChannelStore` impls.

**P1.3 Settle on body completion, not the response header.**
`response_filter` debits on `status.is_success()` (header) — a 200 header then a
mid-stream upstream abort still debits as success. Move the upto settle to
end-of-body.
- Files: `pay` `http402.rs` (settle in a body-end hook, not `response_filter`).

### P2 — economics + observability

**P2.1 MPP-charge refund / scheme policy.** MPP charge is charged-before-serve
with no refund on upstream failure (unlike upto). For fallible endpoints, prefer
upto (refund-on-failure) or add a charge refund path. Revisit the client tie-break
(currently MPP-on-ties) for refundable endpoints.

**P2.2 Stranded-channel observability.** `settle_actual` failure logs only
(`gate.rs:1176`, "left for sweep"). Add a counter+alert and a monitored
sweep-reconcile job.

**P2.3 Idempotency on confirm-timeout.** Broadcast-landed-but-confirm-timed-out
⇒ verify returns error ⇒ client re-challenges ⇒ possible double-pay. Add an
idempotency key keyed on the credential/channel so a retry rejoins the in-flight
settle instead of starting a new one.

**P2.4 Body/size pricing from actual bytes.** Size/token-metered amount uses the
client's `content-length` header (`req.content_length`). Meter from actual bytes.
(No-op for gemini's per-request flat price.)

## 3. Sequencing & the one big decision

P0.1 + P0.2 are independent, low-risk latency wins — do first. P1.1 (optimistic
serve / persistent channel) is the largest win but needs the **policy decision**:
keep confirm-before-serve (safest, slow) vs. serve-after-accepted + async confirm
+ durable reconcile (fast, needs P1.2). P1.2 is a prerequisite for relaxing the
gate and is also a standalone security fix.

Recommended order: **P0.1 → P0.2 → P1.2 → P1.1(a persistent channel) → P1.3 →
P2.x**.
