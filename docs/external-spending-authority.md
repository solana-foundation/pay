# External spending authority for Pay MCP

Pay embedders can optionally consult a runtime spending policy after the SDK has
selected a real payment offer and before Pay starts its signing path. This is an
additive policy check. It does not replace wallet approval or YAML permissions,
and does not provide signature custody, settlement accounting or controlled egress.

## Configure from trusted host context

Override the default `PayContext::spending_authority` method and return an
`authority::AuthorityBinding` with a trusted caller ID and an implementation of
`SpendingAuthority`. The default returns `None`; existing contexts, `pay mcp`,
wallets and YAML permission files keep their current behavior. No external app,
service or additional wallet is required for existing users.

The first extension is programmatic. This PR adds no command runner, HTTP client,
CLI flag or new YAML field. A host can implement a provider using its existing
local IPC or authenticated policy client. Provider failures must remain errors;
an enabled host must not return `None` when its policy service is unavailable.
`None` means deliberately disabled, never unavailable.

```rust,ignore
use std::sync::Arc;
use pay_mcp::{PayContext, CallScope};
use pay_mcp::authority::{AuthorityBinding, SpendingAuthority};
use rmcp::service::{RequestContext, RoleServer};

struct ConnectionContext {
    existing: Arc<dyn PayContext>,
    // Set by the operator/authenticated host, not clientInfo or tool input.
    caller_id: String,
    authority: Arc<dyn SpendingAuthority>,
}
impl PayContext for ConnectionContext {
    fn scope(&self, call: &RequestContext<RoleServer>)
        -> Result<CallScope, rmcp::ErrorData> {
        self.existing.scope(call)
    }
    fn spending_authority(&self, _: &RequestContext<RoleServer>)
        -> Result<Option<AuthorityBinding>, rmcp::ErrorData> {
        Ok(Some(AuthorityBinding::new(
            self.caller_id.clone(), self.authority.clone(),
        )))
    }
}
```

Use one fixed caller per authenticated connection, or derive it from the host's
existing authenticated request mapping. MCP `clientInfo.name` is not identity
proof. The provider and its endpoint are trusted configuration, never tool args.
This is application trust configuration, not protection against a hostile process
that controls Pay's code, environment or host configuration.

## Request and decision

When enabled, `curl` requires `operation_id` (1–128 ASCII letters/digits or `-_.:`).
It remains caller-supplied correlation input, not proof of approval. The trusted
caller ID scopes it; a random server-generated `attempt_id` distinguishes attempts
across connection/process lifetimes.

`SpendingAuthority::authorize(SpendRequest)` returns a nonblocking future resolving
to `AuthorityDecision::Allow`, `Deny`, `RejectBeforeExecution`, or opaque
`AuthorityUnavailable`. `RejectBeforeExecution` is an explicit provider guarantee
that rejection occurred before any durable/economic side effect (including purchase,
reservation, execution provenance, signing or transaction creation). Pay returns
`REJECTED_BEFORE_EXECUTION` without wallet approval or paid HTTP. Only this verdict
permits retrying the same caller/operation/immutable request with a new attempt ID.
Pay cannot prove the absence of provider side effects; an incorrect or dishonest
verdict is a provider contract violation. Generic `Deny` remains fenced.
The default deadline is five seconds, configurable to a nonzero duration <=60s.
Deny, provider error, deadline expiry and configuration failure never fall back to
wallet signing. The future must cooperate with cancellation and must not block
its executor thread. Cancellation cannot undo external provider side effects.

The request contains:

- operation/attempt IDs and trusted caller ID;
- frozen method, URL, normalized headers and snapshotted body bytes;
- atomic amount as a decimal integer string, canonical asset/network/recipient;
- the effective Pay execution-network slug;
- the SDK's full canonical accepted offer (including relevant extra fields).

This is the same request and SDK-decoded offer subsequently used by the original
builder; the callback receives an owned snapshot and cannot rewrite them. It is
not the signed transaction, and does not cryptographically attest what the signer
ultimately signs. Policy implementations must evaluate all relevant facts, not
merchant-supplied description text. Snapshots can contain sensitive application
input: do not log them or forward them to untrusted endpoints.

## Ordering and supported path

`curl` input/body snapshot → connection replay guard → original unpaid fetch and
SDK challenge selection → existing YAML permissions → external authority → existing
hardware/account preparation and wallet-approval/sign/build → existing paid retry
and response interpretation.

The first wired path is Solana x402 V2 exact. Unpaid/free responses do not consult
spending policy. Other classified payment schemes, mixed MPP-preferred offers,
x402 V1 and SIWX signing fail closed when enabled. This PR does not add protocols.

Enabled requests cannot supply Authorization, Cookie, x402 payment or sign-in
headers; stored authenticate/session credentials are not pre-attached. Duplicate
case-insensitive header names are rejected. Neither the probe nor paid retry
follows redirects. An interactive wallet switch is refused instead of applying a
previous decision to a different account. These opt-in restrictions keep other
execution paths from bypassing the check. Disabled mode keeps the old behavior.

### Existing YAML / canonical Mainnet limitation

On the current base, the existing YAML adapter rejects a standard Mainnet CAIP-2
quote: SDK normalization preserves the CAIP-2 identifier in `cluster`, while the
permission adapter converts it through `mainnet-beta` and a short-slug-only parser.
This PR preserves that rejection; it does not skip YAML or rewrite merchant terms.
An independent expected-success reproducer fails on unmodified upstream. The YAML
conjunction tests use the SDK-supported explicit `cluster: mainnet` fixture; they
are not evidence that the canonical-only YAML case works. That production use case
needs a separately reviewed known-network mapping fix, including unknown-network
rejection tests.

## Idempotency and recovery limits

A bounded, mutex-protected connection-local guard claims `(caller_id, operation_id)`
before any HTTP. The immutable request fingerprint remains bound for the entire
connection lifetime, including after `RejectBeforeExecution`. Changed request bytes
always return `OPERATION_CONFLICT` before another probe or authority call.

Each entry is either `Claimed(attempt_id)` or `Retryable`. First start atomically
claims a fresh attempt ID. Concurrent identical calls return
`OPERATION_ALREADY_STARTED`. Only the current attempt owner's explicit
`RejectBeforeExecution` transitions the entry to `Retryable`; the next identical
call atomically claims a new attempt ID. A stale attempt cannot release a newer
claim. No entry is removed. The 1,024-entry limit never evicts old fingerprints;
retrying an existing retryable operation does not consume another entry.

All other provider outcomes retain the claim: generic Deny, Allow (including later
execution failure), unavailable, timeout, malformed response and runtime errors.
Cancellation or a lost response does not itself make an operation retryable. If
Pay never receives a provider verdict (including transport failure, a lost provider
response, or cancellation before a verdict is obtained), the claim stays fenced.
An unpaid/free response or failure before consultation also retains its claim.

Only a `RejectBeforeExecution` actually received and processed by Pay releases the
current claim. An already-running blocking worker can still receive that verdict
after the outer MCP call is cancelled. Once Pay processes the explicit safe
rejection, cancellation of the MCP client or loss of the rejection response on its
way back to that client does not make the provider outcome unknown again: the
operation remains Retryable. Retryability depends on the provider verdict, not on
MCP response delivery.

This is a connection-local policy replay guard, not an economic ledger, cached-result
replay, durable purchase recovery, or exactly-once payment. It resets on restart.
Providers needing cross-process durability must implement their own durable
idempotency, atomically binding caller + operation to immutable request and offer
facts and fencing attempts. Only a prior explicit no-side-effect rejection may
permit a new attempt; uncertain or committed attempts require denial/reconciliation.
A retried probe can select a new offer, which the provider must evaluate again.
Changing operation IDs also requires the provider's own cumulative budget enforcement.

There is no result/receipt callback or original-payment query in this PR. An allow
is not proof of payment, settlement or delivery. A lost ACK, SDK error or timeout
after signing cannot justify a fresh purchase. Providers reserving budgets must
remain conservative until future authoritative outcome/reconciliation interfaces
can account for the original attempt.

## Policy-only mode and separate Controlled Execution integration

The limitations in this guide describe the policy-only External Spending Authority
configuration introduced by #514. In that mode, after Allow, Pay receives valid
credentials and starts paid HTTP. Later revocation/Pause cannot revoke that decision
or those credentials. The policy-only hook does not provide final-send ownership,
durable submission markers, crash-safe reconciliation or exactly-once accounting.

#530 adds the separate optional [Controlled Execution integration](mcp-controlled-execution.md)
via `AuthorityBinding::with_controlled_execution`. It uses the pay-kit unsigned
prepare dependency so a host can own signing and final paid HTTP. Hosts needing
that ownership should use this integration; durable evidence, fencing, Pause checks
and original-payment recovery remain host responsibilities. These are not guarantees
of the policy-only hook itself.
