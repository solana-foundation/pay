# Merchant credit-provider policy

Configure credit providers beside accepted currencies:

```yaml
operator:
  currencies:
    usd: [USDC, USDT]
  credit_providers: []
```

An omitted `credit_providers` field means an empty list. It does not change
existing payment behavior. The policy is merchant-wide and applies before
charge, session or subscription payment handling.

`coinflow` is a recognized provider name, but its integration is not available
yet. Selecting `credit_providers: [coinflow]` fails startup rather than
advertising unsupported funding. Custom Pay middleware states also reject the
configuration before issuing or accepting payment credentials. Unknown provider
names fail deserialization.

The SDK exposes the same enum and availability check as `CreditProvider` and
`validate_credit_providers`. Its `PayKitConfig.credit_providers` field sits
beside currency configuration.

A named provider will select a reviewed integration: supported deployments,
callback entrypoints, account-resolution rules, funding authorization and refund
backing. It is not an alias for arbitrary hook JSON. Coinflow's actual adapter
and preparation contract must be supplied and reviewed before the enum can
enable it.

The raw-hook configuration described in [MPP channel versions](mpp-channel-versions.md)
remains an advanced integration/testing interface. Enabling V2 channel charge
or sessions does not by itself enable a credit provider.

## Musebook (proposed, unavailable)

`credit_providers: [musebook]` is recognized but fails startup. Pay rejects it on
its own, so this does not depend on the pinned pay-kit revision. The name refers
to the reviewed program in
[Solizardking/musebook-credit-provider](https://github.com/Solizardking/musebook-credit-provider).
Object values, hook lists, mixed-case names and `musebook.trade` fail to deserialize,
so customers cannot supply hook JSON.

Planned operator and client behavior once it is enabled (none of this is implemented):

- [ ] The operator resolves hook accounts only through the provider's fixed
      `hook_state(program, owner, mint)`, then emits `openWithHooks` / `topUpWithHooks`
      / `distributeWithHooks` / `withdrawPayerWithHooks` (outer tags 10–13).
      Pay and pay-kit do not emit these yet.
- [ ] The session store records the hook commitment and authorization nonce in
      the same durable Redis transition as the session's open intent.
- [ ] On restart, recovery compares the saved commitment with the on-chain channel
      before it resumes or settles, and fails closed on any mismatch.
- [ ] The client shows the issuer co-signed cap, payee and expiry before the owner signs.
- [ ] Settlement is verified on chain independently of hook callbacks.
