# MPP channel versions

MPP sessions default to the existing V1 payment-channel program. V2 is an
experimental, opt-in deployment with immutable funding and distribution hooks.
Existing channels do not migrate when you change the server configuration.

## Select V2

For a devnet server with MPP sessions already configured, set:

```sh
export PAY_PAYMENT_CHANNELS_PROGRAM_ID=CHNLxBtXw7VPqd5GMcbtVoEbn4ehyaSoiHj56Ao5sWkJ
export PAY_MPP_CHANNEL_VERSION=2
```

Restart the server with its existing devnet configuration. The session challenge
advertises `channelProgram` and `channelVersion`. These settings alone create
ordinary payer-funded V2 channels without hooks; they do not enable Coinflow.
The selected program must be deployed on the configured network.

To keep V1, leave both variables unset. Its program is
`CHNLxYvVA28MJP9PrFuDXccuoGXAx7jBacfLEkahyGsX`. An absent version means V1,
not “infer the version from the program.” Invalid addresses, unsupported
versions and mismatched program/version pairs fail instead of falling back.

## Configure hooks

| Variable | Used by | Value |
| --- | --- | --- |
| `PAY_MPP_CHANNEL_HOOKS` | Merchant server | JSON object with optional `preFund`, `postFund`, `preDistribute`, `postDistribute` descriptors |
| `PAY_MPP_TRUSTED_CHANNEL_HOOKS` | Pay client / MCP | Exact approved descriptor configuration, using the same JSON shape |

Each descriptor contains `programId`, an eight-byte `entrypoint` array,
`accounts` (ordered `{address, writable}` entries), and `staticData` (bytes).
At most four extra accounts and 32 static-data bytes are supported per hook.
There is no extra-account signer flag. Transaction size and compute limits
still apply; not every combination at these per-hook bounds fits a transaction.

Configure only a deployed adapter whose authority and custody rules you have
reviewed. Pay rejects advertised hook configuration before wallet approval
unless it exactly matches the local trusted configuration. Trusting a program
name is insufficient: accounts, permissions and static data must also match.
Even an explicitly advertised empty `{}` configuration requires a matching
client policy; omitting hook configuration requires no hook approval.

Hooks execute as CPIs inside `open`/`topUp` or `distribute`/`withdrawPayer`.
Once installed, they cannot be omitted from later operations. A callback failure
fails the transaction, including its token transfers. A configured distribution
hook still runs when no principal is refunded; a credit adapter must issue no
credits for that case.

## Recovery

Keep the persisted session configuration and channel records. They bind the
program, ABI version and hook policy used at opening. V2 recovery must validate
the account owner and on-chain hook commitment against that stored policy.
Changing process defaults must not reinterpret an existing channel.

V2 session gateways require durable Redis storage: set `PAY_MPP_REDIS_URL`
(or the legacy `PAY_SESSION_REDIS_URL`). Release binaries and the container
image include the `redis-session-store` feature; a source build that drops
default features cannot open Redis. Use the same Redis namespace for the gateway and
settlement worker (`PAY_MPP_REDIS_PREFIX`, falling back to
`PAY_SESSION_REDIS_PREFIX`; default `pay:session:v1:`). The namespace name does
not select the channel ABI. Startup fails rather than falling back to memory.
Configure Redis persistence and backups to retain vouchers and session bindings.

The SDK stores an immutable configuration snapshot for ordinary V2 sessions as
well as deployment-owned sessions. An ordinary binding has a null deployment
policy; the worker still restores and validates its snapshot. Missing or
malformed V2 bindings must not be reconstructed from current defaults or chain
data.

Ordinary V2 and deployment-owned sessions use the same atomic close claims,
owner leases, and heartbeat renewals. Pending setup, live delivery authorizations,
and usage reservations defer close. Once close is claimed, its durable marker
blocks new usage even if the lease expires; another worker can reclaim expired
ownership. Every renewal checks the original policy and configuration snapshot,
so a deleted or replaced binding cannot downgrade the session to unbound V1.
Close and distribution instructions retain the stored V2 program and mandatory
distribution hooks.

Connect currently uses memory-backed session storage and rejects V2 sessions.
Its V1 session behavior is unchanged; this restriction does not disable
standalone V2 channel-backed charge routes.

Chain-only adoption is not supported for V2: public channel state does not
reconstruct the original merchant policy and consumer authentication binding.
Retain access to the V1 deployment and its configuration while V1 channels are
still active.

## Scope

This integration supports MPP sessions and opt-in channel-backed MPP charges.
Without the explicit V2 profile, charges remain direct transfers. x402 behavior
is unchanged. Coinflow transaction preparation and a production
credit-redemption/recredit adapter are not implemented here.
Program hooks alone do not confer permission to debit or mint issuer credits.

## Channel-backed charge routes

With the V2 environment profile above, existing `mpp-charge` routes use a
one-shot payment channel. Keep the same route metering and prices; the CLI
gateway and Connect seller backends advertise the opt-in
`methodDetails.channelCharge` profile. It contains `channelProgram`,
`channelVersion`, `channelHooks`, and the server's `recentSlot`.

Configure an operator transaction signer for the server. For sponsored charges,
the channel payee is the operator identified by `feePayerKey`; the merchant
remains the payout recipient and does not need to provide a private key.
The client checks the exact hook descriptor before requesting wallet approval.
Even a channel-backed charge without hooks requires the explicit client policy:

```sh
export PAY_MPP_TRUSTED_CHANNEL_HOOKS='{}'
```

The client builds the transaction from the challenge's blockhash and slot
without an additional RPC request. A separate ephemeral key signs the voucher;
the wallet signs the transaction. The server retains route-price validation,
challenge authentication, replay rejection, and ordinary `Payment-Receipt`
responses. Hook simulation failures fail closed: channel charges do not retry
with preflight disabled.

The experimental path supports SPL Token payments. Transaction versions follow
the server's advertised capabilities and the wallet's signing support. Hook
profiles that exceed the V0 packet limit require a mutually supported V1
transaction; they cannot bypass packet limits. Unsupported tokens, split
allocations that cannot be represented exactly in basis points, and oversized
transactions fail rather than falling back to a direct transfer. Test the
complete configured hook profile before enabling it for a route.
