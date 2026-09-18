# Keychain and signing backends

How a pay account turns into a signature, and where to add a new custody
provider or hardware wallet.

## Layers

```text
accounts.yml                  Account { backend: BackendKind, provider, auth_required, … }
      │
      ▼  Account::descriptor()
pay_core::backend             SigningBackend  — id, custody, is_exportable, signs_raw_messages, approval
      │                       LocalKeystoreBackend — keystore(params, Gate) for on-machine stores
      │                       remote::RemoteProvider — credentials, discover, connect (Openfort, …)
      ▼
pay_core::signer              ResolvedSigner { backend, Memory | Remote }  implements SolanaSigner
      │
      ▼
pay-keystore                  Keystore = AuthGate + SecretStore  (Touch ID + Keychain, polkit + Secret Service, …)
```

`BackendKind` is only a storage tag persisted as the `keystore` field of
`accounts.yml`. Everything a caller wants to know about what an account can
do comes from its descriptor. There is no `match` on the kind outside
`accounts.rs`, `signer.rs`, and the CLI's account commands, and the kind
never grows a variant for a new remote provider: `keystore: remote` plus a
`provider` string covers all of them.

## Attributes every backend declares

| Method | Meaning | Local stores | Openfort | Ledger (planned) |
| --- | --- | --- | --- | --- |
| `custody()` | Where the key is | `Local` | `Remote` | `Hardware` |
| `is_exportable()` | Raw keypair can be read out (`pay account export`) | yes | no | no |
| `signs_raw_messages()` | `sign_message` verifies over the raw bytes | yes | yes | no (envelope) |
| `approval()` | How a signature is approved | platform prompt, or `op` for 1Password | provider policy behind the platform prompt | device confirmation |
| `is_available()` | Usable on this machine now | OS match, service reachable | always | device attached |
| `max_tx_version()` | Highest transaction version the signer can produce | any | any | V0 (the Solana app does not sign v1 yet) |

All five are required methods. A new backend states what it can do; it does
not inherit a default that may be wrong for it. Custody does not imply the
others: a provider may allow key export, a local store may refuse it, and a
hardware wallet is local but not exportable.

## Where the rules live

- **Export.** `signer::load_keypair_bytes_from_account_*` checks
  `is_exportable()` first and returns one error for every non-exportable
  backend. `pay account export` and the export offer in `pay account destroy`
  both go through it.
- **Approval.** `Gate::for_policy(auth_required, override)` picks the gate:
  a caller-supplied override (MCP elicitation), else the platform prompt,
  else nothing. `LocalKeystoreBackend::keystore` places it in front of the
  secret store. The file backend composes the platform prompt itself; 1Password
  ignores the gate because `op` prompts.
- **Platform `cfg`.** Only inside `backend/local.rs`, one block per store,
  and in `backend::platform()` / `platform_gate()`. Every store exists on
  every OS and reports `is_available() == false` off-platform, so the CLI
  and the remote-credential path never branch on the OS.
- **Registry.** `backend::backends()`, `by_flag`, `local_by_kind`,
  `platform()`. `--backend` flags, the setup picker, legacy `<flag>:<name>`
  signer sources, and `Account::descriptor()` all resolve through it. Apple
  Keychain keeps `keychain` as its flag while its id and `accounts.yml` value
  stay `apple-keychain`.

## Adding a remote provider

1. Add `core/src/remote/<provider>.rs` with a unit struct implementing
   `SigningBackend` (identity and attributes) and `RemoteProvider`
   (credential fields, wallet discovery, connect). See `openfort.rs`.
2. Register it in `remote::PROVIDERS`.

The CLI derives `--backend <id>`, the picker entry, the `{PROVIDER}_{FIELD}`
environment variables, and the credential prompts from the declarations.
Credentials are stored as a blob in the platform store behind the same gate as
a keypair; `accounts.yml` gets `keystore: remote, provider: <id>, account:
<wallet id>, pubkey`.

## Hardware wallets: Ledger

`remote::ledger` (cargo feature `ledger` on pay-core and the CLI) wraps
solana-keychain's `LedgerSigner`. It is a `RemoteProvider` with no credential
fields: nothing is stored in the secret store, `accounts.yml` holds the
derivation path as the wallet id plus the cached address, and the device is
the approval. `pay setup --backend ledger` reads the first two standard
derivation paths and lets the user pick.

Two consequences flow from the attributes rather than from special cases:

- `max_tx_version() == Some(V0)` is threaded into every kit builder (charge,
  x402 exact and upto, channel open), so the kit negotiates v0 with servers
  that advertise v1.
- `signs_raw_messages() == false` makes `SigningBackend::require_raw_message_signing`
  refuse, with the reason, in the paths that sign a raw message: SIWMPP
  authenticate, subscription proofs, operator-signed session proofs,
  x402 sign-in, and batch-settlement vouchers. Charges, x402 exact and upto,
  and client-signed sessions work unchanged.
- The choice of offer follows the same attribute. A 402 often advertises
  an operator-signed session next to a flat charge (the Gemini gateway
  does); the classifier prefers the session and carries the charge as its
  `fallback`. Before paying, the CLI and MCP call
  `RunOutcome::for_account`, which reads the paying account's backend from
  `accounts.yml` without prompting and swaps in the fallback when the
  backend cannot sign the session proof (`session::check_signer`) or an
  x402 sign-in. With no other offer the refusal becomes a `PaymentRejected`
  that names the backend. Verified end to end: a Ledger paid a 0.045 USDC
  Gemini image charge on mainnet after skipping the session.

The feature is opt-in because `hidapi` links IOKit on macOS, hid on Windows
and libudev on Linux. Linux builds need `libudev-dev` and a udev rule for the
device.

## Tests

- `backend/mod.rs`: registry uniqueness, flag and id resolution, kind
  round-trips, per-backend exportability, platform match, foreign stores
  refuse to build, gate policy.
- `backend/local.rs`: file backend needs a path, reads without a gate when
  disabled, applies an override before reading, never prompts on write;
  1Password ignores the gate; legacy flags.
- `signer.rs`: file accounts honour override and policy gates, resolve to a
  local exportable signer; remote accounts refuse raw key access with a
  message naming the provider and custody; missing or unknown provider is a
  config error; ephemeral wallets never prompt; network resolution and lazy
  ephemeral creation unchanged; legacy source prefixes map to backends.
- `remote/mod.rs`: registry, credential storage, provider resolution, a fake
  provider exercising discovery.
- Platform prompts themselves (Touch ID, polkit, Windows Hello, `op`) are not
  exercised in unit tests because they block on a human.
