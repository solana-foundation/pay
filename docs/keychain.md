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
- **Approval.** `Gate::for_policy(auth_required, account.approval_override(override))`
  picks the gate. Without an external `approver`: a caller-supplied override
  (MCP elicitation, a tenant's spend policy), else the platform prompt, else
  nothing. With one, the approver is always asked: alone in place of the
  platform prompt, or after the caller's override, and both must approve.
  `LocalKeystoreBackend::keystore` places the gate in front of the
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

## External approvers (headless hosts)

A host with no screen (CI, cron, a server agent) cannot show Touch ID,
Windows Hello or polkit, so an `auth_required` account there can only fail.
An account can name a program that approves instead:

```yaml
accounts:
  mainnet:
    agent:
      keystore: file
      path: ~/.config/pay/agent.json
      auth_required: true
      approver:
        command: /usr/local/bin/pay-approver
        args: ["--channel", "payments"]   # optional
        timeout_secs: 120                 # optional, default 120
```

`command` must be an absolute path; a bare name such as `pay-approver` or a
relative path is refused, since it would be resolved through the caller's
`PATH` or working directory. The approver starts with a cleared environment
and runs from `/`: it gets `PATH=/usr/bin:/bin`, `HOME` from the user
database (not the caller's `HOME`), and `LANG` only when it is a plain locale
name. Nothing else from pay's environment reaches it, so the caller's
`LD_PRELOAD`, `PATH` or credentials held in environment variables cannot
influence or leak into it. Refer to tools outside `/usr/bin:/bin` by absolute
path, or set `PATH` inside the approver.

For every gated action pay runs the command and writes one JSON request to
its stdin:

```json
{"version":1,"kind":"authorize_payment","message":"authorize payment of $0.05 for accessing API api.example.com","amount_minor_units":500,"moves_money":true,"recurring":false}
```

`amount_minor_units` is USD ten-thousandths (`$1` = `10000`) and is `null`
when the action moves no money. `kind` is one of `authorize_payment`,
`authorize_batch`, `create_account`, `import_account`, `export_account`,
`delete_account`, `open_session`, `use_gateway_fee_payer`, `use_account`.

Exit status `0` approves. Anything else denies, and so does a program that
cannot start, is killed, or does not answer within `timeout_secs` (it is
then killed and reaped, along with any helpers it started on Unix; the
timeout also covers delivering the request). On Unix, anything the approver
leaves running in its process group is killed once it answers, so it should
hand longer work to a separate service.

The approver can't be skipped. It replaces the platform prompt, and when the
caller brings a gate of its own (MCP elicitation, a tenant's spend policy),
both must approve: the caller is asked first, and if it says no, the approver
is not asked at all. This matters on a headless host, where the MCP client
answering an elicitation can be the agent itself. The approver is only
consulted when `auth_required` applies, and it lives in `accounts.yml` next
to `auth_required`, so it is never weaker than turning auth off. A minimal
approver that approves single payments up to $1 and denies everything else:

```sh
#!/bin/sh
req=$(cat)
[ "$(printf '%s' "$req" | jq -r .kind)" = authorize_payment ] || exit 1
printf '%s' "$req" | jq -e '.recurring == false' >/dev/null || exit 1
[ "$(printf '%s' "$req" | jq -r '.amount_minor_units // 99999999')" -le 10000 ]
```

It expects `jq` in `/usr/bin`; where `jq` lives elsewhere (`/usr/local/bin`,
Homebrew), use its absolute path. A missing `jq` makes every check fail, so
the example then denies everything rather than approving.

### Limits

- **The boundary is the OS user.** The approver only constrains an agent that
  runs as a different OS user from the one who owns `accounts.yml`, the
  approver program and the key material. An agent running as the same user
  can edit `accounts.yml` (or remove the approver), replace the approver
  program, or read a file keystore directly. Run the agent as a separate user
  or in a container that cannot write those files.
- **Whoever launches pay controls it.** The process that starts pay sets its
  environment and working directory. It can point pay at a different
  `accounts.yml` (found through `HOME`), change its settings (`PAY_CONFIG`,
  a `pay.toml` in the working directory, other `PAY_*` variables), or load
  code into pay itself with `LD_PRELOAD`. So pay must also be launched by
  something the agent cannot control, for example a system service running
  as another user, not by the agent.
- **The approver file is not checked for who can write it.** If the agent's
  user can modify the approver program or a directory above it, it can
  replace the approver. Keep it in a directory only root or the owner can
  write. A possible follow-up is to refuse such a file, like ssh's
  `StrictModes`.
- **An approval is not bound to the exact transaction.** The request
  describes the action (kind, amount, whether it recurs), but carries no
  recipient, mint or transaction digest, so the approver cannot check that
  what is signed is exactly what it approved.
- **`message` is partly built from what the paid server sends.** Treat it as
  untrusted display text, especially when a person or a model reads it; base
  decisions on `kind`, `amount_minor_units`, `moves_money` and `recurring`.

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
solana-keychain's `LedgerSigner`. The macOS CLI target enables this feature by
default so Homebrew source builds include Ledger support; other targets opt in
with `--features ledger`, and release binaries enable it on every target. It is
a `RemoteProvider` with no credential fields: nothing is stored in the secret
store, `accounts.yml` holds the derivation path as the wallet id plus the cached
address, and the device is the approval. `pay setup --backend ledger` reads the
first two standard derivation paths and lets the user pick.

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

The feature is opt-in outside macOS because `hidapi` links IOKit on macOS, hid
on Windows and libudev on Linux. Linux builds need `libudev-dev` and a udev rule
for the device.

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
