# Controlled execution for MCP hosts

The optional `SpendingAuthority` hook asks whether a selected payment may proceed.
By itself, allow continues through Pay's existing wallet and paid HTTP retry.
A host that owns its signer and final transport can additionally configure
`AuthorityBinding::with_controlled_execution(Arc<dyn ControlledExecution>)`.
This is a programmatic opt-in; the default context, CLI and YAML syntax are unchanged.

After the normal HTTP/402 classifier, SDK offer selection, YAML permissions and
external authority allow, Pay calls the controller on curl's blocking worker:

1. `plan(&ControlledRequest)` returns `Build` or `Replay`.
2. For `Build`, Pay calls pay-kit's existing exact unsigned builder with the host's
   public payer key and the selected requirement. A supplied lifetime must agree
   with any challenge blockhash. The SDK checks a supplied blockhash against the
   host-configured RPC and rejects it if not live on that cluster. Missing
   blockhashes or mint decimals are fetched through that RPC as well.
3. `execute(request, unsigned)` transfers only unsigned bytes to the controller.
   The controller independently validates the message, signs, persists original
   evidence, starts the final paid HTTP and reconciles its original payment.
4. The controller returns only credential-free delivery; Pay runs its existing
   response classifier/interpreter. A `Replay` skips the builder and execution.

The authority's explicit `RejectBeforeExecution` stops before controller `plan`,
unsigned construction or `execute`. It retains the original immutable request
fingerprint and permits the exact same caller/operation/request to begin a new
authority attempt with a fresh attempt ID. A changed request still conflicts.
Deny, unavailable, timeout and unknown provider outcomes remain fenced. Cancellation
or loss of the MCP response alone never releases the claim; a safe rejection already
processed by Pay remains Retryable regardless of MCP response delivery.

After Allow, controller errors or unknown/lost execution replies cannot transition
the operation back to Retryable. Ordinary same-connection retries remain fenced;
original-result recovery remains the host's responsibility.

`ControlledRequest` adds the original selected v2 envelope (including unselected
offers, route metadata and extensions) alongside the unchanged `SpendRequest`.
Existing policy providers and struct literals do not need new fields. The raw
probe uses the same outbound/no-redirect checks as the normal fetch path.

Controller delivery bodies use Pay's existing hosted response limit,
`pay_core::client::fetch::MAX_PUBLIC_RESPONSE_BODY_BYTES` (64 MiB, inclusive),
for both Build and Replay. Hosts must enforce that limit before paid egress;
Pay also checks returned delivery defensively. HTTP status must remain in 100–599.
An invalid status or oversized delivery is terminal and never triggers re-execution.

Controller errors, invalid responses and lost replies are terminal. The controlled
branch returns before Pay account lookup, wallet approval, signing or paid retry.
A controller must never return signed transactions, buyer signatures or payment
headers, including credential reflections in a merchant response. Pay cannot
establish a controller's durable accounting or payment proof from an HTTP 200.

Initial scope is the external-authority path's Solana x402 V2 exact flow. Other
protocols and cached authentication do not fall through to a free wallet path.
Authorization, per-agent grants, cumulative budgets, revocation, signature custody,
durable operation identity and recovery remain host responsibilities. Pay's
connection-local replay guard inherits the spending-authority lifecycle and is not
a durable result store;
reconnect/recovery must query the same original operation in the host.

This interface adds no application-specific bridge, service, ledger, wallet or
management API. It depends on the separate spending-authority hook and pay-kit
unsigned-builder change. No paid integration is required to run its tests.
