# Deployment payment policies: implementation and rollout gates

Status: implemented and locally validated; production enablement remains blocked.

Pay pins `solana-pay-kit` to
`1181a247cc35ba356c7af509c501413c0aee3c57`
([pay-kit #362](https://github.com/solana-foundation/pay-kit/pull/362)).
The locked build no longer needs a local Cargo override.

## Implemented boundaries

- Compute owns versioned policies in an explicitly configured named Firestore
  database. Mutations use preconditions; deletion retains a versioned tombstone.
- The resolver derives deployment ownership from authenticated provider metadata.
  Wallet resolution uses a separate internal proof; compute never receives Privy
  credentials.
- The gateway resolves the policy before a deployment challenge or invocation.
  Missing, expired, deleted, malformed, or mismatched policies fail closed.
  The exact apex retains its separately priced control route.
- Prices stay in integer units. The resolver returns original basis-point splits.
  Delegated payouts that would redirect rounding dust to the operator are rejected.
  The `$0.05` / 30% tax / 10% profit / 60% infrastructure case is exact.
- Channel opening persists a validated intent and immutable policy/payout snapshot
  before funding. Bound and legacy channels cannot be substituted for one another.
- A bounded request-backend cache shares runtime resources; it does not create
  a settlement loop per policy. Deployment construction requires an opaque,
  explicitly connected Redis store and external lifecycle ownership.
- Request reservations and close claims share atomic durable transitions.
  Voucher acceptance checks reservation ownership and deadline in the same update
  as its watermark and debit. Fixed-price service charges only complete successful
  responses; SSE/NDJSON responses are rejected without charge.
- Both HTTP adapters enforce a 300-second absolute request deadline and reject
  ambiguous leading slashes before policy resolution or upstream forwarding.
- Workers restore original payout snapshots after restart, policy update, or
  deletion. Direct payer closure triggers reconciliation independently of idle
  timeout, while active reservations still defer closing.
- Bound settlement uses a direct guarded submission path. Ownership is checked
  after pacing and before each send, including retries. This is not atomic
  Redis/on-chain fencing and cannot recall a transaction already sent.
- Expired unfunded opening attempts become terminal only after sufficient finalized
  evidence. Uncertain and legacy attempts remain recoverable. Terminal records
  retain their binding and do not claim an on-chain seal.

Relevant code:
[policy storage](../rust/crates/mcp-compute/src/payment_service.rs),
[resolver](../rust/crates/core/src/server/deployment_policy.rs),
[shared binding and leases](../rust/crates/types/src/deployment_policy.rs),
[session integration](../rust/crates/core/src/server/session.rs),
[worker](../rust/crates/pay-worker/src/bin/settle_sessions.rs).

## Verification

The combined implementation passed:

- 1,055 kit library tests with server, client, and Redis features, using disposable
  local Redis.
- Pay's selected library/binary suites with the published Git revision and
  `--locked`: CLI, core, proxy, shared types, worker, compute, and wallet.
- Explicit Redis reservation/close contention and durable-constructor tests.
- Strict Clippy for the affected Pay crates and formatting/whitespace checks.

The [generic platform harness](../rust/crates/integration/README.md) additionally
executes funded gateway requests and the actual settlement worker against Redis
and the payment program on an offline local validator. It asserts recipient
balance changes, refunds, persisted policy terms, and no double payment across
competing/repeated worker processes. Run it with `just deployment-test`.

This verifies local program execution, not deployed provider integration or
mainnet settlement. Existing kit-wide Clippy findings are
not represented as a clean strict lint result. Debugger web assets were absent
during local Rust validation; release images must contain the intended UI assets.

## Remaining rollout gates

The proxy rejects deployment-policy environment configuration, and Terraform
rejects enabling `compute_payment_policy_enabled`. Keep those explicit guards
until a reviewed enablement change:

1. Coordinates reviewed kit, Pay, and gateway revisions and compatible images.
2. Verifies Redis persistence, recovery, and the worker's matching namespace.
3. Provisions the internal proof outside source control and confirms private
   service IAM, named Firestore access, and load-balancer-only public ingress.
4. Exercises real funding, policy isolation, failure charging, restart recovery,
   policy update/deletion, payer closure, and original-recipient settlement.
5. Records rollback behavior without silently restoring fleet pricing.

Bound ownership records are retained; bound rent reclamation is not implemented.
Missing ownership blocks fleet orphan cleanup. See the
[worker operations guidance](../rust/crates/pay-worker/README.md).

## Policy document cleanup

With `COMPUTE_PAYMENT_POLICY_DATABASE` configured, the Google compute driver
retires the matching policy before requesting function deletion. Both explicit
deletion and channel resource garbage collection use this path; the MCP wrapper
does not implement a separate Firestore cleanup rule.

Retirement is permanent for that deployment incarnation. Even when no policy
exists yet, the driver writes an identity-only fence so a concurrent first
policy creation cannot race past deletion. Firestore mutations use update-time
preconditions. Provider failures leave retirement intact; an accepted asynchronous
delete or `DELETING` state does not establish that the function is gone.

The independent `reconcile-resources` worker calls the driver's orphan hook.
Verified provider absence or a verified newer incarnation starts seven-day
retention. Reclamation rechecks provider state and conditionally deletes the
old policy document. It does not remove Redis payout snapshots or wallets.

Each pass processes at most 100 documents in ten-document pages, with a
120-second soft budget, per-record deadlines, and bounded metadata-token
retrieval. A separate Firestore checkpoint preserves pagination across runs.
Bad rows are reported without starving later rows. Dry-run performs no writes,
including checkpoint writes. Explicit deletion retries after provider absence
succeed; the independent sweep completes orphan metadata cleanup.

Policy reconciliation is scoped to the configured project, policy region, and
gateway domain. Nondefault-region legacy function deletion is still supported.
Google Functions DELETE has no atomic incarnation precondition: revalidation
protects against changes observed after listing, but cannot eliminate an
out-of-band recreation race between the final provider read and delete.

## External validation workload

The SEC summarizer is a disposable end-to-end use case, not repository product
code. Keep its application source, deployment artifacts, and credentials outside
these repositories. Commit reusable platform fixes and tests revealed by that
exercise, not the application itself. Deployment and funded calls require an
approved environment and spending cap.
