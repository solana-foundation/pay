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

The first extension is programmatic. No command runner, CLI flag or new YAML
field is added. This PR does ship a reference HTTP provider,
`pay_mcp::authority_remote::RemoteHttpAuthority`, whose constructor builds the
HTTP client and whose implementation posts the spend request to your policy
endpoint and maps the JSON verdict (fail closed: timeouts, non-2xx responses,
malformed flags and oversized verdicts all deny the spend). A host can bind it
directly, or implement a provider with its existing local IPC or authenticated
policy client. Provider failures must remain errors; an enabled host must not
return `None` when its policy service is unavailable. `None` means deliberately
disabled, never unavailable.

```rust,ignore
use std::sync::Arc;
use std::time::Duration;
use pay_mcp::authority::{AuthorityBinding, SpendingAuthority};
use pay_mcp::authority_remote::RemoteHttpAuthority;

// Reference provider: point it at your policy endpoint and bind it.
let http_authority: Arc<dyn SpendingAuthority> = Arc::new(
    RemoteHttpAuthority::new("https://policy.internal.example/v1/spend")?
        .with_request_timeout(Duration::from_secs(5)),
);
// ...or keep your own provider (local IPC, authenticated policy client, ...):
let custom_authority: Arc<dyn SpendingAuthority> = my_existing_client;
```

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
to explicit `AuthorityDecision::Allow` or `Deny`, or opaque `AuthorityUnavailable`.
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
before any HTTP. It fingerprints the immutable request. Repeated identical calls
return `OPERATION_ALREADY_STARTED`; changed request bytes return
`OPERATION_CONFLICT`. Neither starts another probe, policy call or signing attempt.
All outcomes, including error/unknown, retain the claim. After 1,024 entries it
fails closed; it never evicts an old operation to authorize a new attempt.

This is at-most-one attempt per connection, not an economic ledger, cached-result
replay or durable purchase recovery. It resets on restart. A provider claiming
durable idempotency must atomically bind caller + operation to immutable request
and offer facts and one attempt ID; a different attempt for an already claimed
operation must be denied/reconciled, never blindly re-approved. That provider
contract is not implemented or guaranteed by this callback API. Changing operation
IDs also requires the provider's own cumulative authorization/budget enforcement.

There is no result/receipt callback or original-payment query in this PR. An allow
is not proof of payment, settlement or delivery. A lost ACK, SDK error or timeout
after signing cannot justify a fresh purchase. Providers reserving budgets must
remain conservative until future authoritative outcome/reconciliation interfaces
can account for the original attempt.

## Controlled Execution remains separate

After allow, Pay still receives valid credentials and starts the paid HTTP request.
A later revocation/Pause cannot revoke that decision or those credentials here.
This PR does **not** implement a last-send gate, unique signer/gate ownership,
signed-evidence persistence, durable submission markers, crash-safe Unknown
reconciliation, or exactly-once accounting. These are blockers for a deployment
requiring strong Pause/custody guarantees, not guarantees provided by this hook.

A later Controlled Execution contract must retain effective credentials and final
HTTP start in a trusted boundary, re-check Pause/generation at that boundary, persist
original evidence/attempts before egress, expose non-consumable handles and safe
results, and reconcile the same transaction after crash/lost ACK. It must also
supply durable ownership/fencing and authoritative outcomes. It should reuse the
SDK and may build on a separate pay-kit unsigned-prepare proposal; this PR has no
such dependency and does not change pay-kit's pin or API.
