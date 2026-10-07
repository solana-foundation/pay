# Platform integration tests

The deployment harness exercises the actual Pingora gateway, Redis-backed
session state, SDK channel opener, payment-channel SBF program, and settlement
worker. It uses a generic HTTP response fixture, not the SEC application.

## Run the deployment flow

Prerequisites: Rust, Python 3.9+, Just, Redis, Surfpool, and `cargo-build-sbf`.
The runner requires a Unix host and does not install system tools.

From the repository root:

```sh
just deployment-test-program
just deployment-test
```

The first command downloads the pinned payment-program source, verifies its
archive digest, applies the same local treasury patch as pay-kit's CI, and builds
with SBF tools `v1.52` / architecture `v1`. Its source reference and program ID
must stay aligned with pay-kit's `build-payment-channels` action. A changed
archive digest requires review, not bypassing the check.

Generated source, the `.so`, and provenance metadata stay under
`rust/target/deployment-e2e/`. **This is a local test artifact, not a release build.**

The second command builds the worker and starts owned Redis and Surfpool processes
on fresh loopback ports. Surfpool runs offline, without a remote datasource.
The fixture seeds the executable ELF, a six-decimal token mint, and initial
test-account balances. Channel creation, charging, sealing, refunds, and
distribution then execute the real program; the fixture does not seed successful
channel or settlement state.

The test has a 180-second overall deadline. The runner stops the test process
group and its owned services on failure, timeout, or interruption. Service logs
are under `rust/target/deployment-e2e/`. HTTP proxy environment variables are
removed, and endpoints must use literal loopback addresses with explicit ports.
The settlement child receives only local test configuration and ephemeral
signing material through its environment, never secret files.

## Assertions

The scenario verifies:

- Two deployment hostnames issue different prices and payout terms.
- SDK-funded opens and authenticated requests deliver the upstream body.
- Cross-deployment channel reuse fails without increasing the debit.
- Upstream errors and unsupported SSE/NDJSON responses are not charged.
- Resolver outages and expired metadata identity tokens fail closed.
- Recreating gateway/session objects retains channel proofs and accounting in Redis.
- Policy version changes reject old channels for new service; deletion stops service.
- With gateway and resolver stopped, separate worker processes restore the original
  payout snapshots and settle the accepted requests.
- Competing workers and a later repeated worker do not pay accepted vouchers twice.
- Recipient token-account balances change by the exact expected amounts, the
  operator retains none of the sellers' tokens, and unused deposits return to the payer.
- Channels reach the actual program's `Distributed` state after seal/distribution.

For the two `$0.05` requests, the original infrastructure/tax/profit wallets receive
`60,000 / 30,000 / 10,000` base units. A different deployment receives `200,000`,
and the updated policy's new recipient receives `75,000`. Original funds do not
move to the updated policy's recipient.

The fixture uses a two-second negotiated idle timeout and one-second close
batching. It waits for persisted deadlines rather than rewriting channel records.
Surfpool's blockhash context can lead its confirmed bank by one slot; the opener
waits for that bank without altering the challenged slot or weakening verification.

## Test boundaries

Policy and metadata HTTP services are controlled fixtures. This test does not
verify Google IAM, Firestore, Privy, DNS, certificates, mainnet execution, or
production Redis persistence. Gateway recreation is not a Redis crash test.
Crash injection around every funding step, HTTP cancellation/deadline tests,
and direct-payer-close races remain separate coverage work.

Production rollout guards remain enabled. The harness explicitly constructs the
gateway, using `pay-core/test-support` only for loopback transport injection.
It retains normal identity-token fetching, audience checks in the fixture,
response validation, deadlines, and bounded bodies. No environment setting
enables this transport injection in production.

Default test selection does not start this harness. The deployment target is gated by
`deployment-e2e`; when selected, missing services are failures, not skipped tests.
For an already owned local environment, the target requires
`PAY_DEPLOYMENT_TEST_RPC_URL`, `PAY_DEPLOYMENT_TEST_REDIS_URL`, and
`PAY_DEPLOYMENT_TEST_WORKER`. Prefer the runner so isolation and cleanup are automatic.

Run the runner's safety tests independently:

```sh
just deployment-test-safety
```

The CI workflow includes a dedicated `Deployment payments (local chain)` job
using these same recipes, pinned Surfpool and Anza tooling, and no cloud credentials.
