# Keystore Security Audit — Triage Notes

Working document for triaging the 71 findings in the Solana Foundation audit of
`pay-keystore`.

- **Source**: `Solana Foundation - keystorecrate(pay)-findings-export-2026-05-15.json`
- **Audit date**: 2026-05-15
- **Repository**: Solana Foundation - keystorecrate(pay)
- **Total findings**: 71 (3 high, several medium, rest low/informational)

Each finding below is one of:

- **resolved** — fix already merged; we either added a regression test or
  pointed to an existing one.
- **fix-in-progress** — being worked on in this branch.
- **partial** — one or more layers of a multi-layer finding are fixed in
  this branch; the remaining layers are tracked as sub-tasks.
- **deferred** — finding acknowledged but the fix is out of scope for this
  branch (typically because it crosses crate boundaries or needs cross-team
  product input). Tracked separately.
- **wontfix** — accepted risk or out of scope (with rationale).
- **open** — not yet triaged.

## Status table

| #   | Severity      | Title                                                                         | Status   |
| --- | ------------- | ----------------------------------------------------------------------------- | -------- |
| 52  | high          | Use of `/tmp` is unsafe                                                       | resolved |
| 28  | high          | macOS helper uses the current directory when `HOME` is empty                  | resolved |
| 2   | high          | Reserved `.pubkey` account names let `pubkey()` return private keypair bytes  | resolved |
| 70  | medium        | macOS trusts PATH for several binaries                                        | resolved |
| 36  | medium        | Gateway fee-payer approval omits server and fee terms                         | deferred |
| 17  | medium        | Session-opening approval omits deposit and operator terms                     | deferred |
| 11  | medium        | Keystore import can leave partial account records after a write failure       | partial  |
| 10  | medium        | 1Password backend trusts a PATH-resolved `op` binary for secret operations    | wontfix  |
| 9   | medium        | SOL send approval omits the transfer amount                                   | resolved |
| 8   | medium        | Keypair import trusts caller-supplied public key bytes                        | resolved |
| 7   | medium        | Keypair loads can use unrelated auth policies from reason text                | resolved |
| 5   | medium        | Keystore imports accept `SyncMode` but never enforce it                       | partial  |
| 4   | medium        | Payment amount parsing can downgrade Linux authentication policy              | resolved |
| 3   | medium        | macOS keystore executes an unpinned cache binary for key operations           | resolved |
| 71  | low           | The embedded `helper.swift` supports only one macOS platform                  | resolved |
| 13  | low           | macOS auth cancellation classification depends on localized text              | resolved |
| 56  | low           | `codesign` doesn't use `--timestamp`                                          | resolved |
| 57  | low           | Child process of `helper_store()` might keep running                          | resolved |
| 58  | informational | case `read-protected` in `helper.swift` is not used                           | resolved |
| 59  | informational | Value of -25244 is not obvious                                                | resolved |
| 60  | low           | Use of `/usr/bin/security`                                                    | resolved |
| 61  | informational | No handling of the errors of `p.run()`                                        | resolved |
| 62  | informational | `interactionNotAllowed` is not required                                       | resolved |
| 63  | low           | No `passcode` fallback                                                        | resolved |
| 67  | informational | The use of `pay.sh` version pay is not consistent                             | resolved |
| 37  | informational | Security note doesn't cover all the nuances                                   | resolved |
| 39  | informational | Static calls used where trait is available                                    | partial  |

(Rows added as we work through findings.)

---

## Per-finding notes

### Native macOS FFI refactor (closes #3, #13, #56–#63, #67, #71)

The macOS backend originally shelled out to a compiled Swift helper
cached at `~/.cache/pay/pay.sh`. The helper itself, the codesign /
swiftc pipeline that built it, the cache directory it lived in, and
every defensive check protecting that cache are now **deleted**.
`pay-keystore` talks to Apple's frameworks directly via Rust FFI:

- `src/macos/keychain.rs` — `SecItemAdd` / `SecItemCopyMatching` /
  `SecItemDelete` / `SecItemUpdate` calls built on
  `security-framework-sys` + `core-foundation`. Same item attributes
  as before (`kSecClass = generic password`, `kSecAttrService =
  "pay.sh"`, `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`), so
  existing Keychain entries load through this module unchanged.
- `src/macos/touchid.rs` — `LAContext.evaluatePolicy` /
  `canEvaluatePolicy` via `objc2-local-authentication`, with the reply
  block bridged through `block2::RcBlock` + an `mpsc::sync_channel`.
- `src/macos/mod.rs` — thin orchestration plus a one-shot
  `cleanup_legacy_helper_once` that removes any leftover
  `~/.cache/pay/pay.sh{,.entitlements}` from older installs.

Removed:

- `src/macos/helper.swift` (131 lines).
- `build.rs` (the entire `swiftc` + `codesign` pipeline plus the
  `OUT_DIR/pay-helper` marker file).
- The helper-cache management Rust code in `src/macos/mod.rs`:
  `helper_path`, `resolve_cache_dir`, `prepare_cache_dir`,
  `validate_cache_dir`, `cached_helper_is_current`,
  `validate_helper_file`, `install_helper`,
  `compile_helper_atomically`, `write_file_atomically`,
  `write_file_exclusive`, `unused_temp_path`, `codesign_binary`,
  `verify_codesign`, `file_equals`, `remove_cached_helper`, plus
  `helper_run`, `helper_store`, `is_user_cancel`, `extract_error`.

Tests reshuffled:

- Dropped 9 file-system tests that were exercising helper-cache
  hardening (`validate_helper_file_*`, `cached_helper_is_current_*`,
  `validate_cache_dir_*`, `resolve_cache_dir_*`,
  `macos_invokes_system_binaries_by_absolute_path`). The surfaces they
  protected no longer exist.
- Added 7 new tests: 3 unit tests on `classify_code` (LAError →
  AuthDenied / Backend), 4 smoke tests on the public macOS module
  (`TouchId::is_available()` doesn't panic, `AppleKeychainStore::exists`
  on an unknown account returns false, plus two for the device-state
  guidance helper).

**Backward compatibility:** keypairs previously written through the
Swift helper are readable through the new code unchanged, because the
Keychain item attribute set (service + account + accessibility) is
identical. There is no upgrade migration required from the user.

**Findings closed by this refactor (deletion makes them inapplicable):**

- **#3** (medium) — no cached helper binary to pin or verify.
- **#71** (low) — no Swift compilation, no per-arch target.
- **#13** (low) — cancellation classification now uses `LAError` codes
  (`userCancel`, `userFallback`, `systemCancel`, `appCancel`,
  `authenticationFailed`) rather than a substring search on the
  localised description string.
- **#56** (low) — no `codesign` invocation.
- **#57** (low) — no child process to manage; FFI is in-process.
- **#58** (informational) — no `read-protected` case to be dead.
- **#59** (informational) — no `-25244` magic number; FFI calls
  return typed `OSStatus` from `security-framework-sys`.
- **#60** (low) — no `/usr/bin/security` shell-out.
- **#61** (informational) — no `Process.run()` error path.
- **#62** (informational) — no `LAContext.interactionNotAllowed`
  dance; our items don't require interactive auth on existence
  probes.
- **#63** (low) — no `LAContext.evaluatePolicy` policy choice to
  argue about (we still use `deviceOwnerAuthenticationWithBiometrics`
  by design; passcode fallback is a follow-up product decision).
- **#67** (informational) — no `pay.sh` version reference to be
  inconsistent.

**Side effect on already-resolved findings:**

- **#28** (high), **#52** (high) — the `resolve_cache_dir` mitigation
  is now moot: there is no cache directory and no helper to attack.
  Status stays `resolved`; the underlying attack surface is gone in
  addition to the mitigations being correct.
- **#70** (medium) — the `swiftc` / `codesign` / `security` PATH
  hijack surface is gone. The `macos_invokes_system_binaries_by_absolute_path`
  regression test was deleted alongside the constants it pinned;
  status stays `resolved` because there are no PATH-resolvable
  invocations left to regress.

### #52 — Use of `/tmp` is unsafe (high) — resolved

**Audit relevantContent (stale):**

```rust
fn helper_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let cache_dir = PathBuf::from(home).join(".cache").join("pay");
```

**Current code** (`src/macos/mod.rs:104-109`):

```rust
fn helper_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| Error::Backend("HOME is required for macOS Keychain helper".to_string()))?;
    let cache_dir = PathBuf::from(home).join(".cache").join("pay");
```

The `/tmp` fallback was removed in commit `ea2aa02` ("fix: keystore audit",
2026-05-01). When `HOME` is unset or empty, `helper_path()` now errors instead
of placing the cached helper under a world-writable directory. This change also
addresses finding #28 ("macOS helper uses the current directory when `HOME` is
empty") via the same `.filter(|home| !home.is_empty())`.

**Defense-in-depth already in place:**

- `prepare_cache_dir` (mod.rs:181) — rejects symlinks at the cache path, sets
  `0o700` on the directory.
- `validate_cache_dir` (mod.rs:216) — not-a-symlink, is-a-directory, owned by
  current `euid`, `mode & 0o077 == 0`.
- `validate_helper_file` (mod.rs:273) — same checks plus rejects hard-linked
  helpers (`nlink != 1`).
- `cached_helper_is_current` (mod.rs:248) — only reuses the cached binary when
  its bytes equal the embedded build artifact, then re-runs `codesign --verify
  --strict`.

**Action taken in this branch:** covered by the #28 fix below (same code
path) — `resolve_cache_dir` is now responsible for HOME validation and is
tested for missing / empty / relative / absolute inputs.

### #28 — macOS helper uses CWD when `HOME` is empty (high) — resolved

**Audit relevantContent (stale):**

```rust
let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
let cache_dir = PathBuf::from(home).join(".cache").join("pay");
```

**Threat model:** if `HOME` was set but empty (or a relative path),
`PathBuf::from(home).join(".cache").join("pay")` resolved under the process
current working directory. A local attacker who controls the CWD could plant
`./cache/pay/pay.sh` there, and a victim Pay process would then spawn it with
key material on stdin during Keychain operations.

**Fix:** Extracted a pure `resolve_cache_dir(home: Option<&OsStr>)`
helper in `src/macos/mod.rs` that rejects missing, empty, and **relative**
`HOME` values before any filesystem lookup. The absolute-path check is the
piece this finding adds on top of the prior `ea2aa02` work.

Downstream defenses (already present, retained):

- `prepare_cache_dir` / `validate_cache_dir` — symlink rejection, ownership,
  `mode & 0o077 == 0`.
- `validate_helper_file` — same, plus hard-link rejection (`nlink == 1`).
- `cached_helper_is_current` — byte-equality with the embedded build
  artifact, then `codesign --verify --strict`.

**Regression tests:** `resolve_cache_dir_rejects_missing_home`,
`resolve_cache_dir_rejects_empty_home`, `resolve_cache_dir_rejects_relative_home`,
`resolve_cache_dir_returns_subpath_of_absolute_home`.

**Not adopted from the recommendation:** the auditor also suggested resolving
the home directory through `getpwuid_r(geteuid())` / `homeDirectoryForCurrentUser`
instead of trusting `HOME`. Skipped because the existing defense-in-depth
(ownership + perms + byte-equality + codesign) already blocks code-execution
even if `HOME` points to an unexpected absolute path. Revisit if a stronger
guarantee is needed.

### #2 — Reserved `.pubkey` account names let `pubkey()` return private keypair bytes (high) — resolved

**Original attack** (from the report):

| Operation                                       | Storage key      | Data                       |
| ----------------------------------------------- | ---------------- | -------------------------- |
| `import_with_intent("victim.pubkey", keypair)`  | `victim.pubkey`  | 64-byte private keypair    |
| `pubkey("victim")`                              | `victim.pubkey`  | expected 32-byte public key |

In the original layout, the private keypair lived at the storage key
`account` and the public-key metadata at `format!("{account}.pubkey")`.
A caller-supplied `victim.pubkey` account therefore aliased the public-key
key for `victim`. Because `pubkey()` was unauthenticated and did not check
the value length, the attacker's 64-byte private keypair could be read out
verbatim through the public-key API.

**Fix** (all three layers must hold to block the attack; current code has them all):

1. **Typed storage prefixes** (`lib.rs:296-302`): `keypair_key` and
   `pubkey_key` prepend `keypair:` and `pubkey:`. The `:` byte is not in
   the allowed account-name charset, so no valid account name can produce
   either typed key.
2. **Reserved-suffix rejection** (`lib.rs:268-272`): `validate_account_name`
   rejects names ending in `.pubkey` (case-insensitive), so the very first
   import call fails before any storage write.
3. **Size check on read** (`lib.rs:205-210`): `pubkey()` now runs
   `validate_pubkey` after load and rejects anything other than 32 bytes —
   defense-in-depth for corrupted or future-bug storage state.

**Regression tests** in `lib.rs`:

- `reserved_pubkey_suffix_is_rejected` — unit test for layer 2.
- `pubkey_rejects_private_keypair_sized_value` — unit test for layer 3
  (bypasses validation by writing directly to the underlying store, then
  asserts `pubkey()` refuses to return 64 bytes).
- `typed_storage_keys_do_not_alias_valid_account_names` — unit test for
  layer 1.
- `audit_2_pubkey_collision_attack_is_blocked` — new end-to-end test that
  walks the auditor's exact narrative: legit `victim` import → attacker
  attempts `victim.pubkey` → rejection + `pubkey("victim")` still returns
  the legitimate public bytes, never the attacker's. Covers case variants
  of the suffix too.

**Failure demonstrated:** temporarily disabling the reserved-suffix check
makes `audit_2_pubkey_collision_attack_is_blocked` fail on the import
assertion, confirming the test catches that regression.

### #70 — macOS trusts PATH for several binaries (medium) — resolved

The macOS backend invokes three system binaries while installing and
verifying the Swift helper. Each is now resolved by absolute path so a
local attacker who controls earlier `PATH` entries cannot substitute a
hostile binary:

- `swiftc` → `/usr/bin/swiftc` (constants `SWIFTC` in `src/macos/mod.rs`
  and `build.rs`)
- `codesign` → `/usr/bin/codesign` (constants `CODESIGN` in
  `src/macos/mod.rs` and `build.rs`)
- `security` → `/usr/bin/security` (`URL(fileURLWithPath:)` in
  `src/macos/helper.swift`)

No bare-name `Command::new("swiftc"|"codesign"|"security")` invocations
remain in the crate.

**Regression test:** `macos_invokes_system_binaries_by_absolute_path`
asserts the two Rust constants start with `/` and that the embedded
Swift source literally contains `URL(fileURLWithPath: "/usr/bin/security")`.
A future edit that reintroduces a PATH-based lookup for any of the three
binaries will fail the test.

**Failure demonstrated:** temporarily flipping `SWIFTC` to `"swiftc"`
fails the test on the absolute-path assertion.

**Not in scope of this finding:** the 1Password `op` CLI is still
PATH-resolved (`store::OnePasswordStore`); that is finding #10 and tracked
separately.

### #36 — Gateway fee-payer approval omits server and fee terms (medium) — deferred

`AuthIntent::use_gateway_fee_payer()` returns a unit-style variant with a
hard-coded "use your pay account as the gateway fee payer" message. The
intent has no fields for network, server identity, recipient/operator
pubkey, pull-mode flag, fee budget, lifetime, or server-wide caching, so
the OS auth prompt is identical for a sandbox gateway, a mainnet public
gateway, and a pull-mode operator setup. On Linux the static
`sh.pay.use-gateway-fee-payer` Polkit action carries no amount bucket,
unlike the per-tier `AuthIntent::AuthorizePayment` actions.

Three live call sites in `rust/crates/cli/src/commands/server/start.rs`:

- `:288` — default-account fallback when no `operator.signer` block is
  configured.
- `:1268` — `SignerConfig::Account` loader.
- `:1277` — `SignerConfig::File` loader.

**Why deferred:** the fix crosses the keystore → cli boundary, requires a
new structured `GatewayFeePayerTerms` type, a per-bucket Polkit action set
in `rust/config/polkit/sh.pay.unlock-keypair.policy`, and CLI plumbing to
populate the terms at each call site. Best done as its own PR alongside
#17 (same architectural pattern), with explicit product decisions on:

- whether to require an explicit fee cap / session lifetime, and what the
  default lifetime should be when the gateway operator doesn't set one;
- whether to add a Pay-controlled confirmation dialog when terms cannot
  be displayed safely in the OS prompt.

**Planned scope:** see #17 for the shared design notes — both findings will
land together.

### #17 — Session-opening approval omits deposit and operator terms (medium) — deferred

`AuthIntent::open_session()` is structurally the same problem as #36 — a
unit-style variant with a hard-coded "authorize opening a pay session"
message. The single call site in
`rust/crates/core/src/client/session.rs:250` (inside
`open_pull_session_header`) already has `request.operator`,
`request.currency` (the resolved mint), the resolved `network`, and the
caller-supplied `deposit` in scope, but none of them are passed into the
intent.

**Why deferred:** the architectural change (extend the variant to carry a
structured terms type, render those terms into the prompt, add a
deposit-bucket Polkit action set) is parallel to the #36 work and is
cleanest to land together. Doing it standalone first is viable but the
duplication isn't worth it.

**Planned scope, shared with #36:**

1. Replace the `OpenSession(String)` and `UseGatewayFeePayer(String)`
   variants with structs that carry the audit-requested fields:
   - session: `{ network, operator, mint, deposit_micro_usdc, user_pubkey? }`
   - gateway fee payer: `{ network, server, recipient?, pull_mode, fee_budget?, session_lifetime?, cached_for_server }`
2. Build prompt text from the terms (network + concrete identifiers +
   cap / lifetime) using the existing truncation helpers, like
   `authorize_payment_details` does today.
3. Linux Polkit: map the deposit / fee-budget to a `PaymentLimit` bucket
   and route to per-bucket actions
   (`sh.pay.open-session-up-to-usd-*`, `sh.pay.use-gateway-fee-payer-up-to-usd-*`),
   mirroring `polkit_payment_limit_action`. Add the new action ids to
   `rust/config/polkit/sh.pay.unlock-keypair.policy`.
4. Thread the values from the call sites:
   - `core/src/client/session.rs:250`
   - `cli/src/commands/server/start.rs:288`, `:1268`, `:1277`
5. Tests: unit tests on intent rendering + Polkit mapping (mirroring the
   existing payment-intent tests). End-to-end CLI test is hard because
   server startup mocks aren't there yet — keep it scoped to the intent +
   mapping layer.

**Out of this finding's scope:** the auditor also recommends renewing
approval on lifetime expiry or cap exhaustion. That's a server runtime
behavior, not a keystore concern; tracked separately if/when it lands.

### #11 — Keystore import can leave partial account records after a write failure (medium) — partial

The audit calls out three independent hazards that can leave the API
result out of sync with the durable backend state. They're tracked here
as sub-tasks so the backend-specific work is visible.

**11-A (core split-write rollback) — resolved.**

`Keystore::import_with_intent` writes two backend records: the 64-byte
keypair and the 32-byte pubkey. If the second write failed, the API
returned `Err` while the keypair was already committed. `lib.rs:176-184`
now performs a best-effort `delete(keypair_key(account))` when the pubkey
write fails, so the post-call state matches the returned result for every
backend (the fix lives above the `SecretStore` trait, so all current and
future stores get it).

Regression test: `import_rolls_back_keypair_when_pubkey_write_fails` uses
a `FailOnSecondStore` mock that commits the first write and errors on the
second. Currently passes; verified failing against the pre-fix version of
`import_with_intent`.

**11-B (1Password delete-before-create) — deferred (tracked with #6).**

`OnePasswordStore::store` (`store.rs:217-218`) deletes the existing item
before creating the new one. If the create fails (CLI not signed in,
network blip, etc.), the previous account is gone. The auditor's
recommended path — `op item edit` on the existing item, falling back to
`op item create` for new ones, with verification after the write — has
significant overlap with #6 (the 1Password argv-leak finding) because
both require changing the secret-transport contract with the `op` CLI.
Best landed in a dedicated `op`-backend PR alongside #6.

**11-C (macOS Keychain delete-before-add) — deferred.**

`helper.swift:doStore` calls `SecItemDelete` and then `SecItemAdd`. The
recommended fix is to prefer `SecItemUpdate` when the item already
exists, and fall back to `SecItemAdd` when it does not. Small enough to
land standalone; deferred because it requires touching the embedded
Swift helper and re-running the build-time codesign path, which is best
tested on a macOS workstation in isolation rather than mixed into the
core-rollback commit.

### #10 — 1Password backend trusts a PATH-resolved `op` binary for secret operations (medium) — wontfix

The 1Password (`op` CLI) backend is being removed from `pay-keystore`.
Once the `store::OnePasswordStore` / `store::OnePasswordAuth` types and
their `op` invocations are gone, this finding ceases to apply.

The same reasoning extends to **every 1Password-specific finding** in the
audit (#6, #14, #24, #27, #29, #32, #41, #43) and to sub-item **#11-B**
above — all of them become inapplicable once the backend is removed.
They will be batch-closed in `security_report.md` when the removal lands.

### #9 — SOL send approval omits the transfer amount (medium) — resolved (by removing the dead code path)

The auditor reports that `AuthIntent::send_sol(recipient)` builds the SOL
transfer prompt without an amount or `PaymentLimit`, so the OS auth
prompt cannot tell the user whether they are approving 0.1 SOL, 10 SOL,
or a drain. The auditor also says the caller is
`pay_core::client::send::send_sol()`.

**Status of the alleged caller:** there is no `pay_core::client::send::send_sol`
function in the current tree. `crates/core/src/client/send.rs` only exposes
`send_stablecoin`, `parse_token_amount`, and `format_token_amount`. Direct
SOL transfers go through the stablecoin path (different intent shape).

**Status of `AuthIntent::send_sol`:** the only references to it were the
constructor definition in `auth.rs:160` and a single Polkit-mapping unit
test assertion in `linux/mod.rs:349`. No production caller exists in the
workspace.

**Fix applied:** removed the dead `AuthIntent::send_sol` constructor.
Also removed the `"authorize sending"` prefix branch from
`AuthIntent::from_reason` and the corresponding unit test — that prefix
was reachable only through `send_sol`-shaped messages, so it had no live
producer either.

If a real SOL-transfer flow is added later, build the intent from the
canonical recipient *and* the resolved lamport amount (use
`AuthIntent::authorize_payment_details` or extend `authorize_payment` to
carry a SOL `PaymentLimit` bucket), per the auditor's recommended shape.

### #8 — Keypair import trusts caller-supplied public key bytes (medium) — resolved

`validate_keypair` previously only checked that the imported buffer was
64 bytes long. A caller could supply `[secret_seed_A || pubkey_B]` where
`pubkey_B` was unrelated to `secret_seed_A`, and the keystore would
record that account with the wrong public-key metadata. `pubkey()` would
later return `pubkey_B` and Pay could display or sign with the wrong
account identity.

**Fix** (`lib.rs`):

- Added `ed25519-dalek` to the crate's dependencies (already a workspace
  dep).
- `validate_keypair` now returns `Result<[u8; PUBKEY_LEN]>`: it interprets
  bytes `0..32` as the Ed25519 signing seed, derives the matching
  `VerifyingKey`, and rejects the import if it does not byte-equal the
  caller-supplied `32..64` half.
- `import_with_intent` uses the *derived* pubkey (not the caller-supplied
  bytes) for the pubkey-metadata write, so the stored identity always
  comes from the validated signing key.
- `load_keypair_with_intent` was already calling `validate_keypair`, so
  stored records that disagree with the secret half are now also rejected
  on read — defense-in-depth against direct backend tampering.

**Backward compatibility:** Solana keypairs produced by `solana-keygen`,
by `pay setup`, or by `pay-core`'s own `SigningKey::generate(...).to_keypair_bytes()`
always have matched halves and import / load identically before and
after the fix. The only thing that breaks is what the audit asked for:
imports with mismatched halves.

**Regression test:** `import_rejects_mismatched_pubkey_bytes` imports a
`[0xAA; 32] || [0xBB; 32]` buffer (length-valid, derivation-invalid) and
asserts the import is rejected and no record is left behind. Failed
against the pre-fix `validate_keypair`; passes after the derivation
check is added.

**Test fixture cleanup:** added `make_keypair(seed_byte)` and
`pubkey_for(seed_byte)` helpers in the tests module. Existing tests that
used `[0xAA; 32] || [0xBB; 32]`-style buffers now use the helpers, so
all imports go through the new derivation check. No production caller
code changed.

### #7 — Keypair loads can use unrelated auth policies from reason text (medium) — resolved

`Keystore::load_keypair(account, reason)` is a key-read operation. The
previous implementation routed `reason` through `AuthIntent::from_reason`,
whose prefix-matching could classify the text as `DeleteAccount`,
`AuthorizePayment`, `ImportAccount`, `OpenSession`, etc. Each of those
variants maps to a different Linux Polkit action, so caller-controlled
prose could shift the policy bucket for a key-read into something else
entirely (a per-amount payment action for `"authorize payment of $0.0001
for loading the victim keypair"`, the delete-account action for
`"delete the \"victim\" payment account"`, and so on).

**Fix** (`lib.rs:229-239`): `load_keypair(reason)` now always builds an
`AuthIntent::use_account(reason)`. The text still appears verbatim in the
OS prompt; only the operation classification is pinned. The typed
`load_keypair_with_intent` API is unchanged and remains the supported
path for callers that need a specific operation class.

**Regression test:** `load_keypair_does_not_inherit_privileged_intent_from_reason`
uses a `RecordingAuth` mock that captures the intent passed to
`authenticate`. After a real-keypair import, it calls `load_keypair` with
both the auditor's exact delete-shaped example (`"delete the \"victim\"
payment account"`) and a payment-shaped reason, and asserts the captured
intent is `UseAccount` in both cases. Failed against the pre-fix
`from_reason` routing (which yielded `DeleteAccount` for the first
example); passes after the fix.

**Production callers checked:** every production caller in this workspace
(`pay-core`, `pay-cli`) already uses the typed `*_with_intent` APIs.
`load_keypair(reason)` is only exercised from the keystore crate's own
tests. The same prefix-matching shape exists in `Keystore::delete(reason)`,
`Keystore::authenticate(reason)`, and `Keystore::import_with_reason(reason)`
— those convenience entry points are also test-only, and the auditor's
narrow recommendation was about `load_keypair`. If we want to harden the
public surface further, the cleanest follow-up is to delete those
reason-string conveniences entirely and require typed intents at the API
boundary; tracked as a non-finding follow-up.

### #5 — Keystore imports accept `SyncMode` but never enforce it (medium) — partial

`SyncMode` was a two-variant enum, but the import code discarded it
(`_sync` parameter) and the `SecretStore` trait never received it. A
caller asking for `CloudSync` against a device-only backend (or
`ThisDeviceOnly` against 1Password, which is inherently synced through
the 1Password cloud) silently fell back to the backend's default
behavior — the API appeared to honor a storage policy that nothing
enforced.

**Action this branch — minimal fix per product direction:**

- Commented out the `CloudSync` variant in
  `rust/crates/keystore/src/lib.rs:36-43`. The variant is left in place
  as a comment with a `Do NOT re-enable without …` note so the future
  cloud-sync work has a clear marker. Only `ThisDeviceOnly` is
  constructible today, so callers can no longer request a mode the
  keystore does not enforce.
- Removed the two CLI sites that previously chose `CloudSync` for the
  1Password backend
  (`rust/crates/cli/src/commands/account/import.rs:82` and
  `rust/crates/cli/src/commands/account/new.rs:98`). Both now pass
  `ThisDeviceOnly` unconditionally; the 1Password branch was redundant
  anyway given that backend is being removed (see #10).
- No `SecretStore` trait change. The `_sync` parameter on
  `Keystore::import*` keeps its underscore — accurate today because the
  one remaining variant matches every backend's actual behavior.

**Why "partial":** the auditor's broader recommendation is to thread
`SyncMode` into `SecretStore` so each backend can declare which modes it
supports and fail-closed on a mismatch. That contract change is the
right shape once cloud sync is a real product feature (e.g. macOS
`kSecAttrAccessibleWhenUnlocked` + `kSecAttrSynchronizable`). Deferring
that work until the feature is real; the current commenting-out keeps
the API honest in the meantime.

**Regression test:** the existing
`sync_mode_default_is_this_device_only` assertion is still meaningful
and stays. No new test needed: with only one variant, the previous
"silently accepts an unsupported mode" failure mode is unreachable by
construction.

### #39 — Static calls used where trait is available (informational) — partial

`Keystore::gnome_keyring_available` and `Keystore::windows_hello_available`
were calling the inherent `is_available` method on the concrete type, which
would silently diverge if the trait signature ever changed. Switched both
to explicit trait-method dispatch:

- `SecretStore::is_available(&linux::SecretServiceStore)` (`src/lib.rs`)
- `AuthGate::is_available(&windows::WindowsHelloAuth)` (`src/lib.rs`)

**Why partial:** the third call site flagged by the auditor —
`Keystore::onepassword_available` — is the 1Password backend, which is
being removed (see #10). Tracked with that removal rather than touched
here.

No regression test: this is a static-dispatch shape change that the
compiler will catch if the traits diverge.

### #37 — Security note doesn't cover all the nuances (informational) — resolved

The crate-level `Keystore` doc previously had a one-paragraph security
note that did not distinguish backends or threat classes. The auditor
asked for an explicit matrix that calls out which threats each backend
actually blocks.

**Fix** (`src/lib.rs:46-95`): expanded the `# Security note` into a
threat-by-backend table covering the four scenarios the auditor lists
(different OS user, same-user process, unlocked physical access,
locked physical access) for macOS / Linux / Windows, plus a
per-backend caveat block. Notable verified facts:

- macOS items use `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`,
  which gates on screen-unlock but **not** biometric presence. Cross-
  references audit #1 for the rationale on not setting
  `kSecAttrAccessControl`.
- Linux "locked physical access" coverage depends on the desktop
  relocking the keyring; this is true under default GNOME but not
  guaranteed elsewhere.
- All backends are "not blocked" for same-user processes — the
  [`AuthGate`](src/auth.rs) prompt is the only barrier and is
  bypassable by a co-resident program calling the underlying
  Secret Service / Keychain / Credential Manager directly.

No code change; doc-only.

### #4 — Payment amount parsing can downgrade Linux authentication policy (medium) — resolved

The auditor identified three coupled defects in the amount → Polkit
action pipeline:

1. **Parse-failure downgrade** — when `limit` was `None`, the Linux
   backend fell back to the *generic* `sh.pay.authorize-payment` action,
   which is *less* restrictive than the per-bucket actions.
2. **Comma truncation in prose parser** — `payment_limit_from_message`
   walked the message with
   `take_while(|ch| ch.is_ascii_digit() || *ch == '$' || *ch == '.')`,
   so a `"$50,000"` message was truncated to `"$50"` and classified as
   the `Usd50` bucket.
3. **Free-form prose driving policy** — `AuthIntent::from_reason` called
   `payment_limit_from_message` on caller-supplied prose. Per the
   auditor: stop deriving policy from display text.

**Fix** (three small changes that compose):

- `src/auth.rs` — `AuthIntent::from_reason` now sets `limit: None` for
  any prose-derived `AuthorizePayment` and `payment_limit_from_message`
  has been deleted. Prose can still flow through as display text; it
  never selects a payment bucket.
- `src/linux/mod.rs` — `polkit_action_for_intent` for
  `AuthorizePayment { limit: None }` now maps to
  `sh.pay.authorize-payment-above-usd-50` (the most restrictive bucket)
  instead of the generic action. Combined with #1, any unparseable
  amount (commas, locale formatting, malformed input, prose) requests
  the strictest policy — failing closed.
- The `POLKIT_ACTION_PAYMENT` constant was removed; its sole consumer
  was the unwrap_or above. The generic action ID still exists in the
  installed policy file as a catch-all and is reachable via the
  `LEGACY_POLKIT_ACTION` missing-action fallback, but it is no longer
  the default for unparseable amounts.

**Regression tests:**

- `audit_4_comma_formatted_amount_does_not_downgrade_limit` (`auth.rs`)
  — `AuthIntent::from_reason("authorize payment of $50,000 ...")` now
  yields `payment_limit() == None`. Failed against the pre-fix prose
  parser (`Some(Usd50)`).
- `audit_4_unparseable_amount_maps_to_most_restrictive_polkit_action`
  (`linux/mod.rs`, gated to Linux builds) — `AuthorizePayment { limit:
  None }` routes to the AboveUsd50 action.
- `audit_4_typed_payment_with_comma_amount_uses_restrictive_bucket`
  (`linux/mod.rs`) — `AuthIntent::authorize_payment("$50,000", "...")`
  routes to AboveUsd50 (the typed constructor's
  `PaymentLimit::from_amount("$50,000")` already returned `None`; the
  fix is in how `None` is now routed).

**Behavior change for `default_payment`:** `AuthIntent::default_payment()`
(used by `pay-core::signer` when no concrete amount is supplied) now
maps to the AboveUsd50 Polkit action on Linux instead of the generic
one. This is the auditor's "fail closed" direction; the existing
`LEGACY_POLKIT_ACTION` fallback ensures backward compat with policy
files that don't have the per-bucket actions installed.

**Parser swap-in (`rust_decimal`):** `parse_usd_minor_units` now uses
`rust_decimal::Decimal::from_str` (added to the workspace as a new
direct dep, `default-features = false, features = ["std"]`). That gives
us strict numeric parsing for free: commas, locale formatting,
embedded whitespace, double-decimals, and non-numeric suffixes all
return `Err` and route to `None`. We additionally reject negative
values, a leading `+`, and scientific notation (`e` / `E`) — the last
because the amount string flows into the OS auth prompt verbatim and
`"$1.0e3"` would look broken in the prompt even though it's a valid
`Decimal`. The hand-rolled `fractional_units` helper is deleted; ceil
rounding is now `Decimal::ceil()` on the scaled value.

`audit_4_amount_parser_rejects_malformed_inputs` pins the strict
behavior so a future parser change can't quietly loosen it.
