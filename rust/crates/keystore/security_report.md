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
| 38  | informational | `is_available()` functions called inconsistently                              | resolved |
| 40  | informational | `lock()` errors not detected                                                  | resolved |
| 19  | low           | `hex_decode` can panic on non-ASCII input                                     | resolved |
| 26  | low           | Keystore existence probes skip account-name validation                        | resolved |
| 20  | low           | Import convenience API authenticates with a create-account intent             | resolved |
| 34  | low           | Keystore load APIs trust malformed backend record lengths                     | resolved |
| 12  | low           | Delete can report success while leaving stale public-key metadata             | resolved |
| 25  | low           | Concurrent keystore mutations can desynchronize keypair and public-key records | partial |
| 16  | low           | Windows account names differing only by case share Credential Manager targets | resolved |
| 1   | informational | macOS Keychain helper exposes private key commands without item-level authentication | resolved-with-rationale |
| 23  | low           | macOS auth reason leaks through helper process arguments                      | resolved-by-ffi |
| 51  | informational | Function `helper_path()` could cache results                                  | resolved-by-ffi |
| 53  | informational | Use of `#[cfg(unix)]` is redundant                                            | resolved-by-ffi |
| 54  | low           | Errors with `set_permissions()` are ignored                                   | resolved-by-ffi |
| 55  | low           | Order of signing addresses                                                    | resolved-by-ffi |
| 48  | informational | The use of number 19 in `process_start_time()` isn't obvious                  | resolved |
| 49  | informational | Several public rust object can be shielded (Linux part)                       | resolved |
| 44  | informational | The linux `AuthGate.is_available` doesn't check the Polkit action exists      | resolved |
| 47  | informational | Errors in `polkit_authenticate()` don't show all situations                   | resolved |
| 35  | low           | Linux Polkit falls back to a generic prompt when specific actions are missing | resolved |
| 46  | low           | Checks on error texts or zbus are not robust                                  | resolved |
| 22  | informational | GNOME Keyring exposes keypairs without the Pay Polkit gate                    | resolved-with-rationale |
| 33  | low           | Linux Secret Service default-collection fallback is inconsistent              | resolved |
| 31  | low           | Linux Secret Service relock failure can mask a completed mutation             | resolved |
| 50  | informational | The `lock()`s on linux too aggressive                                         | resolved |
| 30  | low           | Linux keystore calls can panic inside Tokio runtime contexts                  | resolved |
| 45  | informational | `polkit.message` could be used to display custom messages                     | resolved |
| 64  | informational | Errors of the `COM` initialization are not caught                             | resolved |
| 68  | low           | Windows: Immutable pointers casted to mutable pointers                        | resolved |
| 69  | low           | In windows, the function `cred_read()` isn't sufficiently hardened            | resolved |
| 65  | informational | Old windows API used                                                          | resolved |
| 66  | low           | `GetForegroundWindow()` returns the window with keyboard focus at the time of the call | resolved |
| 15  | low           | Windows Hello unavailable states are reported as user-denied auth             | resolved |
| 21  | informational | Windows Credential Manager exposes keypairs without Windows Hello             | deferred |

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

### #21 — Windows Credential Manager exposes keypairs without Windows Hello (informational) — deferred

Symmetrical to audit #1 (macOS) and #22 (Linux). The auditor
observes that the stored credential under
`pay.sh/<account>` is read back by `CredReadW` without invoking
Windows Hello — any same-user process can call `CredReadW`
directly and bypass `WindowsHelloAuth::authenticate`. The Windows
Hello gate is a per-call wrapper, not an item-level binding.

**Why deferred:**

Binding the stored credential to Windows Hello requires switching
the storage path from raw Credential Manager
(`CredWriteW` / `CredReadW`) to **DPAPI-NG** (Data Protection API
Next Gen) with a Windows Hello protector — the equivalent of the
macOS biometric-ACL question (audit #1) and Linux Secret Service
item-binding (audit #22). DPAPI-NG protectors are out of scope for
this audit cycle because:

- The migration would need a one-shot re-encrypt step for every
  existing user's stored keypair, which means handling old + new
  side-by-side until everyone has rotated.
- DPAPI-NG with a Windows Hello protector requires testing across
  the Windows Hello configurations (PIN-only, fingerprint, face,
  device-bound TPM key), which we can't reasonably exercise from
  the macOS host this audit is running on.
- The current `WindowsHelloAuth` gate, combined with the
  per-account write lock from audit #25 and the typed intent
  routing from audit #7, gives the same user-presence guarantee at
  the call site as the macOS LA-gate model (which we accepted as
  resolved-with-rationale in #1).

**Revisit if:** we ship a Windows-supported release and have a
Windows test environment, or a security event makes the
in-process bypass concrete.

### #15 — Windows Hello unavailable states are reported as user-denied auth (low) — resolved

`WindowsHelloAuth::authenticate` collapsed every non-success
`UserConsentVerificationResult` into `Error::AuthDenied`. That
includes device-state outcomes (`DeviceBusy`, `DisabledByPolicy`,
`NotConfiguredForUser`) which are not user-attempted denials —
they're configuration / hardware availability issues that need
admin or operator action, not "try again."

**Fix** (`src/windows/mod.rs` — `WindowsHelloAuth::authenticate`):
split the match into two error classes that the audit explicitly
asked for:

| Result                       | Error class | Reason                               |
| ---------------------------- | ----------- | ------------------------------------ |
| `Verified`                   | `Ok(())`    | success                              |
| `Canceled`                   | `AuthDenied`| user cancelled                       |
| `RetriesExhausted`           | `AuthDenied`| user exhausted attempts              |
| `DeviceBusy`                 | `Backend`   | device-state, not a user denial      |
| `DisabledByPolicy`           | `Backend`   | admin policy blocks Hello entirely   |
| `NotConfiguredForUser`       | `Backend`   | needs Settings setup                 |
| _ (unknown variant)          | `Backend`   | future-proof default                 |

Callers can branch on `Error::Backend` (device-state) vs
`Error::AuthDenied` (user-attempt) to show the right next step
instead of generically nudging "try again."

### #66 — `GetForegroundWindow()` returns whatever has keyboard focus (low) — resolved

`prompt_parent_window` used the result of
`GetForegroundWindow()` directly as the parent HWND for the
Windows Hello prompt. That returns whichever window has keyboard
focus *at the instant of the call* — could be the user's browser,
another terminal, or any random app. Attaching our Touch-ID
prompt to an unrelated window is confusing at best, and at worst
a UI-redress hazard.

**Fix** (`src/windows/mod.rs` — `prompt_parent_window`): after
`GetForegroundWindow()`, call `GetWindowThreadProcessId` on the
returned HWND and only use it as the parent if the PID matches
`std::process::id()`. If it doesn't, fall through to `None`,
which routes the prompt through the UWP
`UserConsentVerifier::RequestVerificationAsync` path — that
renders the prompt without a parent window, but with the correct
Windows Hello chrome.

### #65 — Old windows API used (informational) — resolved

`IUserConsentVerifierInterop` was hand-rolled with
`windows::core::imp::define_interface!`, a manual `IInspectable_Vtbl`
layout, a per-call IID argument, and `transmute_copy(message)`
to coerce the `HSTRING` into the COM ABI shape. The `windows` crate
ships a typed `Win32::System::WinRT::IUserConsentVerifierInterop`
with a `RequestVerificationForWindowAsync` method that handles all
of that internally.

**Fix** (`src/windows/mod.rs`, `Cargo.toml`):

- Added `Win32_System_WinRT` to the `windows` dep features.
- Replaced the entire hand-rolled interface block (interface
  definition, vtable struct, `Deref`-to-`IInspectable` impl,
  `interface_hierarchy!` invocation, manual transmutes) with the
  upstream import: `use windows::Win32::System::WinRT::IUserConsentVerifierInterop`.
- `request_verification_for_window_async` now calls
  `interop.RequestVerificationForWindowAsync(hwnd, message)`
  directly. No `transmute`, no `transmute_copy`, no manual vtable.

Net: -45 lines, including all the unsafe transmutes the audit
flagged as risky.

### #69 — `cred_read()` isn't sufficiently hardened (low) — resolved

The auditor listed four concerns about `cred_read`:

1. **No null-check before `&*ptr`** — `CredReadW` can in theory
   return `Ok` with a null out-pointer; dereferencing would UB.
2. **`CredentialBlobSize as usize` without validation** — DWORD is
   `u32`; cast is widening on 64-bit, identity on 32-bit. Not
   strictly an overflow, but a sanity check against the "size > 0
   with null blob pointer" mismatch was missing.
3. **`slice::from_raw_parts` alignment** — `*const u8` has
   alignment 1, so this is not a concrete bug, but the audit
   asked for explicit handling.
4. **`CredFree` not called if `to_vec()` panics** — leak on
   OOM-style unwinds.

**Fix** (`src/windows/mod.rs`):

- New RAII guard `CredentialGuard(*mut CREDENTIALW)` that calls
  `CredFree` in `Drop`. `Drop` runs on every unwind path, so the
  allocation never leaks even if `to_vec()` panics.
- `cred_read` now wraps the returned pointer in the guard, refuses
  to dereference a null pointer (returns `Error::Backend`),
  validates `CredentialBlobSize == 0 ⇒ empty vec`, and refuses to
  slice when `CredentialBlobSize > 0 && CredentialBlob.is_null()`.
- `cred_exists` also routes through the guard, replacing the
  ad-hoc "`if found && !ptr.is_null() { CredFree(...) }`" branch.
- Alignment is documented as no-op (u8 alignment is 1).

### #68 — Windows: Immutable pointers casted to mutable pointers (low) — resolved

`cred_write` cast `&[u16]` and `&[u8]` slices to `*mut` via
`as_ptr().cast_mut()` to satisfy `CREDENTIALW`'s pointer types.
That tells Windows the buffers are writable even though the slice
itself may have been a static or otherwise read-only memory region
(`include_bytes!`, string literal, etc.). Even when Windows doesn't
actually write, the compiler may miscompile around the `&[u8]`
immutability assumption.

**Fix** (`src/windows/mod.rs` — `cred_write`): copy `target` and
`blob` into local `Vec`s up front, then hand `as_mut_ptr()` to
`CREDENTIALW`. The pointers Windows receives now genuinely point at
writable memory owned by the function frame; callers can pass any
slice shape without aliasing concerns.

The same shape applies to `cred_read` / `cred_delete` only at the
`PCWSTR(target.as_ptr())` call sites, which are `const` Windows
APIs — they take `*const` pointers and the cast is sound. No
change needed there.

### #64 — Errors of the `COM` initialization are not caught (informational) — resolved

`ensure_com_init` dropped the `CoInitializeEx` result on the floor
(`let _ = ...`). A genuine COM-init failure surfaced later as a
confusing WinRT call error in `request_verification` or
`CheckAvailabilityAsync` rather than at its source.

**Fix** (`src/windows/mod.rs`):

- `ensure_com_init` now returns `Result<()>`. On failure it
  propagates an `Error::Backend("COM init failed: {hresult}")`.
- `AuthGate::authenticate` propagates the init error with `?`.
- `WindowsHelloAuth::is_available` treats an init failure as
  "not available" (the safer answer) — callers branching on it
  expect a bool, and a backend that can't initialize COM cannot
  meaningfully say "available".

The per-thread `OnceLock`-style cell semantics are preserved: only
the first call does the real work; subsequent calls return Ok(())
without retrying the HRESULT.

### #45 — `polkit.message` could be used to display custom messages (informational) — resolved

The auditor pointed out that polkit's `details` map accepts a
well-known `polkit.message` key that GNOME, KDE, and MATE polkit
agents will display in the prompt. Pay was passing an empty
`details` map, so the prompt fell back to the static
`<description>` from the policy file. The policy file's text can
only describe the *action class* (e.g. "Pay session approval"),
not the per-call intent details Pay's typed `AuthIntent` already
carries.

**Fix** (`src/linux/mod.rs`):

- `Polkit::authenticate` forwards `intent.message().to_owned()`
  into a new `polkit_authenticate_with_message` helper.
- That helper inserts `polkit.message → intent.message()` into the
  `details` map. Empty messages are skipped so agents fall back to
  the policy file's per-action `<description>`.
- The original `polkit_authenticate(action, interactive)` is kept
  as a thin wrapper for callers that don't have an intent in scope
  (the non-interactive probe in `Polkit::is_available`).

Cross-agent behavior: polkit agents that ignore the key still
display the policy-file text, so the change is monotonic — better
when supported, no worse when not.

### #30 — Linux keystore calls can panic inside Tokio runtime contexts (low) — resolved

The shared `run(...)` helper built a new current-thread Tokio
runtime and called `block_on` directly. Tokio panics if you
`block_on` from a thread that's already inside a runtime. `pay
server start` triggers exactly that path: the main server runtime
calls `Keystore::load_keypair_with_intent` during signer
resolution, which hits `SecretServiceStore::load`, which hits
`run(...)`.

**Fix** (`src/linux/mod.rs`): detect a live runtime via
`tokio::runtime::Handle::try_current()`. If one is active, move the
runtime construction onto a dedicated OS thread via
`std::thread::scope` — the inner runtime is created off the
caller's runtime thread, so Tokio's "you can't block_on from inside
a runtime" check no longer applies. If no runtime is active (CLI
invocations, tests), the helper builds inline as before.

Added `Send` bounds on the future and return type because the
in-runtime path crosses a thread boundary. Both the `secret-service`
futures and our own state are already `Send` (`String`, `Vec<u8>`,
`Zeroizing<Vec<u8>>`).

### #50 — Linux locks are too aggressive (informational) — resolved

`SecretServiceStore::store|load|delete` always called
`col.lock().await` after the inner work, even when the user had
left the `pay` collection unlocked on purpose. That can interfere
with another app in the same session that's also using the keyring
— a per-Pay-operation forced relock looks like a flap.

**Fix** (`src/linux/mod.rs`):

- `ensure_unlocked` now returns `Result<bool>` — `true` if we
  unlocked the collection, `false` if it was already open.
- Each store/load/delete path captures that boolean and only
  performs the trailing `col.lock().await` when it was previously
  locked. Combined with audit #31's best-effort relock semantics,
  Pay now leaves the user's keyring in the state the user had set,
  not the state Pay's own per-operation policy would prefer.

### #31 — Linux Secret Service relock failure can mask a completed mutation (low) — resolved

`SecretServiceStore::store` (and the parallel paths in `load` and
`delete`) saved the result of the inner operation, then called
`col.lock().await.map_err(ss_err)?` before returning. If the inner
mutation succeeded but the relock failed, the call returned the
relock error — telling the caller the operation failed while the
backend already held the new state. The shared
`Keystore::import_with_intent` would then halt before writing the
`.pubkey` record, leaving a keypair-only account record on disk.

**Fix** (`src/linux/mod.rs`): switched the three relock sites to
best-effort (`let _ = col.lock().await;`). The collection ends up
in whatever state Secret Service managed to leave it in; the
mutation/load result is what the caller sees, matching the durable
state. If the user's session policy demands a tight relock window,
the relock can still be re-attempted by the next operation (which
re-runs `ensure_unlocked` + `lock`).

Side benefit: pairs with audit #50 (overly aggressive relock) —
both are nudging Pay toward leaving the user's keyring in the state
the user expected, not bouncing it around per Pay operation.

### #33 — Linux Secret Service default-collection fallback is inconsistent (low) — resolved

`SecretServiceStore` had three different policies for the `pay`
collection vs the Secret Service default collection:

- `load` — required the `pay` collection; errored otherwise.
- `exists` — searched the `pay` collection if present, fell back to
  the default collection if not.
- `delete` — searched both collections regardless.

That asymmetry let `exists("alice")` return `true` for an account
that lived in the default collection but that `load("alice")` would
then refuse to read. The audit asked for one rule.

**Fix** (`src/linux/mod.rs` — `exists`): only probe the `pay`
collection. If the user hasn't run `pay setup` yet, `exists` returns
`false` (consistent with `load`'s "pay keyring not found" error).
The default-collection fallback in `delete` is retained on purpose:
delete is a cleanup path, not a read path, so the asymmetry there
is intentional transitional housekeeping.

### #22 — GNOME Keyring exposes keypairs without the Pay Polkit gate (informational) — resolved-with-rationale

The auditor observes that the Secret Service item stored under the
`pay` collection is a plain generic-password record. The
[`Polkit`](src/linux/mod.rs) auth gate is a per-call wrapper in
front of the store — any program with a Secret Service connection
can call `search_items(service="pay.sh", account=…)` and read the
keypair bytes without invoking polkit. Symmetrical to audit #1 on
macOS and #21 on Windows.

**Decision: accept with rationale, no code change.**

Secret Service doesn't expose item-level authorization hooks that
we could bind to a Polkit action. The freedesktop spec gives us:

- **Collection-level lock** — already used; relies on the user's
  login keyring being relocked when the desktop session locks.
  Documented under the audit #37 threat-matrix expansion.
- **Item attributes** — visible to anyone with read access; not an
  authorization layer.
- **Item secrets** — encrypted in transit, but any caller in the
  same session can ask for them.

No mainstream Linux desktop ships a Secret Service implementation
that gates reads on a specific PolicyKit action. Implementing that
would require either patching `gnome-keyring-daemon` (out of scope
for this repo) or replacing the store with a custom Pay-only daemon
(huge platform surface; the user already runs the polkit gate, which
catches the common case of an unattended same-user process trying
to use the key).

**Mitigations actually in place today:**

- `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` analog: the `pay`
  collection is relocked after each operation
  (`src/linux/mod.rs:store|load|delete` — `col.lock().await.map_err(ss_err)?`).
- `Polkit::authenticate` runs before every keystore mutation /
  load path via `Keystore::{import,delete,load_keypair}_with_intent`
  — covers our own callers; doesn't bind co-resident programs.
- `Keystore::gnome_keyring_available` (audit #38, #44) now
  requires both polkit and Secret Service to be reachable, so
  callers can't silently end up on a half-configured backend.

**Revisit if:** a future freedesktop Secret Service spec exposes
per-action ACLs (no current LWN-tracked work in this direction), or
we ship our own Pay-aware daemon for Linux.

### #46 — Checks on error texts or zbus are not robust (low) — resolved

The "polkit action is missing" classifier in `polkit_authenticate`
matched substrings on the localized error text
(`"No such action"`, `"not registered"`). PolicyKit translates these
strings per-locale and the wording changes across versions; on a
French desktop or a future polkit release, a real missing-action
error would have been reported as a generic backend error.

**Fix** (`src/linux/mod.rs`): switched the classifier to match on
the structured `zbus::Error::MethodError(OwnedErrorName, _, _)`
pattern. PolicyKit raises the missing-action condition with the
typed name `org.freedesktop.PolicyKit1.Error.NoSuchAction`; older
daemons sometimes use `org.freedesktop.DBus.Error.Failed` with the
same semantic body. Either name → `missing_action_error(action)`;
everything else stays in the `Backend(format!("polkit: {e}"))`
catch-all. Names are stable across locales and major versions.

### #35 — Linux Polkit falls back to a generic prompt when specific actions are missing (low) — resolved

`Polkit::authenticate` previously fell back to
`sh.pay.unlock-keypair` (the legacy catch-all action) whenever the
typed action (per-amount payment bucket, delete/import/session/
gateway-fee-payer) returned `is_missing_action`. That silently
**downgraded** the authorization an admin or policy engine saw: an
administrator who tightened the per-bucket action could be bypassed
through the legacy generic action without realizing it.

**Fix** (`src/linux/mod.rs` — `Polkit::authenticate`): removed the
fallback match arm. If the typed action isn't installed, the
structured `is not installed` error surfaces unchanged to the
caller, so the operator can fix their policy file or notice a
mismatched policy set.

**Why the legacy action stays defined:** `LEGACY_POLKIT_ACTION` is
still referenced as the probe target in `Polkit::is_available`
(audit #44) — that's a non-interactive availability check, not an
authorization fallback, so it doesn't change which action the
authorization is granted against.

**UX note:** users on a host with only the legacy action installed
will now get a clear "polkit action 'sh.pay.…' is not installed"
error from `pay setup`-installable paths, with instructions to
reinstall the policy file. The previous silent fallback was
convenient for fresh installs but defeated the per-bucket policy
work done in audit #4.

### #47 — Errors in `polkit_authenticate()` don't show all situations (informational) — resolved

`polkit_authenticate` collapsed every non-authorized polkit reply
into `Error::AuthDenied("authentication cancelled")`, even when the
user wasn't actually challenged. PolicyKit's reply already carries a
`challenge: bool` flag that distinguishes "user cancelled the
prompt" from "policy refused to challenge" — the latter happens when
the action's `allow_active`/`allow_inactive` is set to `no` or an
admin rule denies the action outright.

**Fix** (`src/linux/mod.rs`): destructured the tuple's `challenge`
field and split the error:

- `authorized=true` → `Ok(())`
- `authorized=false, challenge=true` →
  `AuthDenied("authentication cancelled")` — caller can prompt user
  to try again.
- `authorized=false, challenge=false` →
  `AuthDenied("not authorized by polkit policy")` — retrying is
  useless; caller should surface the policy-config issue.

### #44 — Linux `AuthGate.is_available` doesn't check Polkit action exists (informational) — resolved

`Polkit::is_available` only probed the system bus
(`zbus::Connection::system().await.is_ok()`). Even when the polkit
action was missing, it returned `true` — the failure then showed up
at the next interactive `authenticate()` call as a structured
"polkit action is not installed" error.

**Fix** (`src/linux/mod.rs`):

- Parametrized `polkit_authenticate(action, interactive: bool)`.
  PolicyKit's `CheckAuthorizationFlags` bit 0 is
  `AllowUserInteraction`; `interactive=false` clears it so the
  caller gets the authorized/challenge tuple back without blocking
  on a user prompt.
- `Polkit::is_available` now drives a non-interactive
  `CheckAuthorization` against `LEGACY_POLKIT_ACTION` (the catch-all
  installed by `pay setup`). Anything that isn't a missing-action
  error (Ok / AuthDenied / other backend) means the action is
  reachable.
- Existing interactive callers in `Polkit::authenticate` pass
  `interactive=true` unchanged.

Audit #38 already composes `Polkit::is_available` into
`Keystore::gnome_keyring_available`, so the new probe now flows
through to the public availability shim as well.

### #49 — Several public rust object can be shielded (Linux part) — resolved

The auditor flagged that several `pub` types could be `pub(crate)`,
since they're only used internally by the keystore. Letting external
crates hand-roll `Keystore::new(NoAuth, Polkit, …)` defeats the
intended auth coupling.

**Linux scope of this commit:**

- `linux::Polkit` → `pub(crate)` (`src/linux/mod.rs`). No external
  user references it.
- `linux::SecretServiceStore` → `pub(crate)` (`src/linux/mod.rs`).
  Previously referenced by `pay-core::signer::Keystore::GnomeKeyring`
  branch, which built `Keystore::new(NoAuth, SecretServiceStore, false)`
  by hand. That call site now uses a sanctioned constructor:
- New `Keystore::gnome_keyring_no_auth()` (`src/lib.rs`) — returns a
  `Keystore` with `NoAuth + SecretServiceStore`. Used by pay-core for
  the "account doesn't require per-network auth" branch.

**Out of Linux scope:**

- macOS visibility (`TouchId`, `AppleKeychainStore`) — tracked under
  the same finding but lives in the macOS module; treated in the
  macOS queue if/when a similar tightening is wanted. The current
  macOS path doesn't have an external user that would break.
- 1Password (`OnePasswordAuth`, `OnePasswordStore`) — being removed
  (#10), no point tightening visibility.
- Windows (`WindowsHelloAuth`, `WindowsCredentialStore`) — handled
  in the Windows queue if applicable.

### #48 — The use of number 19 in `process_start_time()` isn't obvious (informational) — resolved

`process_start_time` parsed `/proc/self/stat` by hand and indexed
field 19. The `19` is the offset of `starttime` in the post-`comm`
field list per proc(5); not obvious without the man page in hand,
and the file format has gained fields in newer kernels (so the index
also has to drift).

**Fix** (`src/linux/mod.rs`, `Cargo.toml`): added `procfs = "0.17"`
under the `cfg(target_os = "linux")` target dependencies and switched
`process_start_time` to `Process::myself().stat().starttime`. The
named field reads the same value, drops the magic number, and lets
`procfs` track any future kernel additions to the `stat` line.

### #55 — Order of signing addresses (low) — resolved-by-ffi

The auditor flagged the `codesign` signer-prefix ordering used by
the keystore's build script: it tried `Developer ID Application`
first, then fell back to `Apple Development`. For local builds the
recommended order is the reverse — local/team certificates first,
distribution certificates only when present.

**Post-FFI status:** the keystore's `build.rs` and its
`codesign_binary` helper are deleted. Nothing in the pay-keystore
crate calls `codesign` anymore. The macOS Justfile codesign step
added later (see root `Justfile` `install pay` recipe) is an ad-hoc
sign with `--sign -`, which doesn't consult signer prefixes at all
— it just stamps the binary with a content hash. The ordering
question no longer applies.

### #54 — Errors with `set_permissions()` are ignored (low) — resolved-by-ffi

The auditor pointed out that `set_permissions()` errors on the
cached helper file were ignored. Because the cached helper carried
private key bytes, a silent permissions failure could leave it
group/world-readable on an NFS-style mount, undermining the
defense-in-depth around the cache directory.

**Post-FFI status:** there is no cached helper file. `set_permissions`,
the `.cache/pay/pay.sh` install path, the `0o700` directory hardening
pass, and `validate_helper_file`'s mode check are all deleted (commit
c27c622). Storage and load happen in-process via Security.framework;
there is no on-disk artifact for the user to set permissions on.

### #53 — Use of `#[cfg(unix)]` is redundant (informational) — resolved-by-ffi

The auditor noted that `#[cfg(unix)]` annotations inside the macOS
module were redundant — macOS is always Unix. The recommendation was
to drop them.

**Verified:** `grep -rn '#\[cfg(unix)\]' rust/crates/keystore/src/macos/`
returns zero matches in the post-FFI tree. The annotations lived on
filesystem-permissions code paths that were deleted alongside the
Swift helper (commit c27c622). Nothing to remove.

### #51 — Function `helper_path()` could cache results (informational) — resolved-by-ffi

The auditor noted that `helper_path()` was called on every
`helper_run()` / `helper_store()` invocation and did two expensive
things: a filesystem check (`binary.exists()`) and a
`codesign --verify --strict` subprocess spawn. The recommendation
was to cache the result with `LazyLock`.

**Post-FFI status:** `helper_path`, `helper_run`, `helper_store`, the
codesign-verify call, and the entire on-disk helper they protected
have been deleted (commit c27c622). There is no per-call filesystem
or subprocess hot path left to cache. The remaining one-shot work in
the macOS module is `cleanup_legacy_helper_once`, which already uses
`OnceLock` to fire exactly once per process.

### #23 — macOS auth reason leaks through helper process arguments (low) — resolved-by-ffi

**Audit relevantContent (stale):**

```rust
let output = Command::new(&binary)
    .args(["authenticate", &message])
    .output()
```

The auditor's concern: the user-facing approval reason (which can
contain payment amounts, recipients, account names) was passed to
the Swift helper as a command-line argument. Process metadata like
`ps -ef` exposes argv to any same-user observer while the child is
running. A wall-of-shame-grade privacy leak, not a key compromise,
but still worth fixing.

**Post-FFI status:** there is no child process. The native FFI in
`src/macos/touchid.rs:43-47` passes the reason as an in-process
`NSString` to `LAContext.evaluatePolicy`:

```rust
let reason = NSString::from_str(reason);
unsafe {
    ctx.evaluatePolicy_localizedReason_reply(
        LAPolicy::DeviceOwnerAuthenticationWithBiometrics,
        &reason,
        &block,
    );
}
```

Nothing escapes to argv, the environment, or stdin/stdout, and no
external observer sees the reason text outside the Touch ID prompt
itself.

### #1 — macOS Keychain helper exposes private key commands without item-level authentication (informational) — resolved-with-rationale

The auditor observes that the macOS Keychain item is stored with
`kSecAttrAccessibleWhenUnlockedThisDeviceOnly` only — there is no
`kSecAttrAccessControl` binding the read to biometric presence. Touch
ID is enforced through a separate `LAContext.evaluatePolicy` gate
before each load. A program running as the same user with the screen
unlocked could therefore call `SecItemCopyMatching` directly and
bypass our auth gate.

**Decision: keep the current LA-as-separate-gate model.**

The recommended fix —
`SecAccessControlCreateWithFlags(.biometryCurrentSet)` plus
`kSecAttrAccessControl` on the stored item — works correctly only if
the calling binary has a **stable code-signing identity**. The
Keychain ACL keys on the binary's designated requirement (derived
from its code signature). Without an Apple Developer ID + team
identifier:

- Ad-hoc signatures hash the binary bytes; the requirement changes on
  every rebuild.
- The user would be re-prompted with the "Allow / Always Allow" login-
  password dialog after every `pay` upgrade or rebuild — strictly
  worse UX than today, and a habituation hazard if the user reflex-
  clicks "Always Allow."
- A genuine attacker on the same machine can already trigger our
  auth gate via UI automation, so the attack model the ACL would
  defend against ("co-resident process bypasses LA gate") is roughly
  the same threat shape as "co-resident process drives the UI."

**Mitigations actually in place today:**

- `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` — Keychain refuses
  reads while the screen is locked; item never syncs to iCloud and
  doesn't appear in keychain backups.
- `LAContext.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, ...)`
  in `src/macos/touchid.rs:43-47` — biometrics-only (no passcode
  fallback), called immediately before every `load_keypair_with_intent`
  via `Keystore::load_keypair_with_intent` (`src/lib.rs`).
- The post-FFI implementation no longer exposes "raw" `read` /
  `delete` commands as a child-process surface (the Swift helper is
  gone — see audit #3 / #71). Storage and load happen in-process
  behind `AppleKeychainStore::{store, load, delete}`.
- The `pay` binary is ad-hoc codesigned at install time
  (`just install pay` → `codesign --sign - --force …` in the root
  `Justfile`), which gives the Keychain a stable identity for the
  installed binary on this Mac (cross-machine identity still requires
  a real Developer ID, which we do not have).

**Revisit if:** we ship a Developer ID Application-signed `pay`
binary. At that point the ACL becomes durable across releases and
the trade-off flips — we can move to
`SecAccessControlCreateWithFlags(.biometryCurrentSet)` and keep the
LA gate as defense in depth (or remove it once the ACL covers the
same threat).

### #16 — Windows account names differing only by case share Credential Manager targets (low) — resolved

The shared `validate_account_name` accepted ASCII letters in either
case, so `Default` and `default` were both valid logical names. The
Windows backend's `cred_write` / `cred_read` use `pay.sh/<key>` as
the Credential Manager target, which folds case — those two names
collide on Windows but stay distinct on macOS / Linux. Same logical
input, two different durable states across backends.

**Fix** (`src/lib.rs` — `validate_account_name`): tightened the
allowed character set to `a-z 0-9 . _ -` (lowercase letters only).
Every backend now sees the same uniqueness contract; the error
message that users see also matches the actual allowed set, which
previously claimed lowercase only while quietly accepting uppercase
too.

**Backward compatibility:** any account that was created with
mixed-case characters (e.g. `MyAccount`) will fail the validator on
the next call to `import`, `delete`, `pubkey`, `load_keypair`, or
`exists`. The stored bytes remain on the backend; users can re-import
under a lowercase name to recover. We do not silently lowercase the
input because that would change which storage key the backend reads
from — a quiet redirect to a different (possibly attacker-planted)
record is worse than the explicit "rename your account" error.

**Regression test** (`lib.rs` tests module):
`validate_rejects_uppercase_letters` — covers leading-uppercase,
all-uppercase, mixed-case, and confirms lowercase + allowed
punctuation continues to work.

### #25 — Concurrent keystore mutations can desynchronize keypair and public-key records (low) — partial

The keystore stores one logical account as two backend records
(`keypair:<name>` + `pubkey:<name>`). `import_with_intent` writes them
in two separate calls; `delete_with_intent` removes them in two
separate calls. With no synchronization, two same-account operations
from different threads could interleave to produce a state no single
successful operation could (e.g. `keypair = T1` + `pubkey = T2`).

**Fix** (`src/lib.rs`): added a per-account `Mutex` map on `Keystore`
itself. `import_with_intent` and `delete_with_intent` acquire the
per-account lock around the two-store sequence, serializing
same-account mutations within the process. The lock is acquired
**after** auth/validation so an unauthorized caller can't induce
serialization side-channels on other accounts.

The mutex map is keyed by account name; entries are created lazily
and never removed (they're zero-sized internally, so the memory
footprint is one `Arc<Mutex<()>>` per imported account name — fine in
practice).

**Why partial:** the audit's framing was specifically about
desynchronized records, which the per-account lock prevents within
the process. Cross-process concurrency (two `pay` processes racing
on the same backend) is **not** addressed by this fix and is out of
scope for the current backends — Apple Keychain, Secret Service, and
Credential Manager don't expose transactional primitives that would
let us atomic-swap two records. Documenting this limit in the
struct doc-comment.

**Regression test** (`lib.rs` tests module):
`concurrent_imports_leave_records_consistent` — 50 rounds of two
threads importing different keypair bytes under the same account
name. After each round, the test loads both records and asserts the
pubkey matches the keypair that "won" the race. Without the per-
account lock, the assertion fails under ~50 rounds reliably (the two
writes are short and interleave easily on a multi-core host).

### #12 — Delete can report success while leaving stale public-key metadata (low) — resolved

`Keystore::delete_with_intent` removed the keypair record, then ran
the pubkey-metadata delete with the result discarded
(`let _ = self.store.delete(&pubkey_key(account));`). If the pubkey
delete failed, the API still returned `Ok(())` while leaving the
keystore in a split state: `exists()` returned `false` because the
keypair was gone, while `pubkey()` could still return the stale
metadata.

**Fix** (`src/lib.rs` — `Keystore::delete_with_intent`): propagate the
second `delete` result. The function returns `Err` if either leg
fails, so the API result honestly reflects the durable state.

**Idempotency check across backends:**

- `InMemoryStore` — `HashMap::remove` returns `Option`, we ignore
  the value and return `Ok`. Idempotent.
- macOS Keychain (`keychain.rs`) — already treats
  `errSecItemNotFound` as `Ok`. Idempotent.
- Linux Secret Service (`linux/mod.rs`) — iterates matching items;
  an empty match list returns `Ok`. Idempotent.
- Windows Credential Manager — `CredDeleteW` currently errors on
  missing items; tracked as audit #18 and fixed in the Windows
  queue. Once that fix lands, re-running `delete_with_intent` on an
  already-deleted account stays `Ok`.

**Regression test** (`lib.rs` tests module):
`delete_surfaces_pubkey_record_failure` — uses a `FailOnNthDeleteStore`
mock that errors on the second `delete` call. After an import, the
test asserts `Keystore::delete` surfaces the error instead of
swallowing it.

### #34 — Keystore load APIs trust malformed backend record lengths (low) — resolved

The audit calls out that `Keystore::pubkey` and
`load_keypair_with_intent` could return whatever bytes the backend
held, without validating that the buffer was the documented length.

**Current code already validates** (added with the audit #2 / #8 fixes
in earlier commits): `pubkey()` runs `validate_pubkey` (32-byte length
check); `load_keypair_with_intent` runs `validate_keypair` (64-byte
length **plus** the seed-to-pubkey derivation check from audit #8, so
even a length-valid but mismatched 64-byte buffer is rejected).

**Pinning regression tests** (`lib.rs` tests module):

- `pubkey_rejects_truncated_backend_record` — plants a 16-byte record
  under `pubkey_key("victim")` directly via the store and asserts
  `pubkey()` returns `InvalidKeypair`.
- `load_keypair_rejects_malformed_backend_record` — plants a 48-byte
  record (length-wrong) and a 64-byte all-`0xAA` record (length-OK,
  derivation-wrong) and asserts both return `InvalidKeypair`.
- The existing `pubkey_rejects_private_keypair_sized_value` already
  covered the 64-bytes-under-pubkey-key case (audit #2).

These three tests collectively pin every shape the audit flagged.

### #20 — Import convenience API authenticates with a create-account intent (low) — resolved

`Keystore::import()` is the public convenience wrapper for importing
an existing 64-byte keypair. It authenticated with
`AuthIntent::create_account(account)` — a different operation class
from the `AuthIntent::import_account(account)` constructor already
defined alongside it. On Linux that distinction maps to two separate
Polkit actions (`sh.pay.create-account` vs `sh.pay.import-account`),
so callers using the convenience API got the wrong approval class.

**Fix** (`src/lib.rs` — `Keystore::import`): switched the convenience
constructor to `AuthIntent::import_account(account)`. Callers that
need explicit control still go through `import_with_intent`.

**Regression test** (`lib.rs` tests module):
`import_uses_import_account_intent_not_create_account` — uses the
existing `RecordingAuth` mock to capture the intent passed to
`authenticate`, then asserts it matches `AuthIntent::ImportAccount`.

### #26 — Keystore existence probes skip account-name validation (low) — resolved

**Audit relevantContent (stale):**

```rust
pub fn exists(&self, account: &str) -> bool {
    self.store.exists(&keypair_key(account))
}
```

**Current code** (`src/lib.rs` — `Keystore::exists`):

```rust
pub fn exists(&self, account: &str) -> bool {
    validate_account_name(account).is_ok() && self.store.exists(&keypair_key(account))
}
```

The validation guard was added in commit `ea2aa021` (2026-05-01), two
weeks before the audit was published (2026-05-15) but evidently
captured against an earlier snapshot. With the guard in place,
`exists("bad/name")`, `exists("")`, and `exists("victim.pubkey")` all
short-circuit to `false` without touching the backend, matching the
behavior of the other typed APIs (`import`, `pubkey`, `delete`,
`load_keypair`).

**Regression test** (`lib.rs` tests module):
`exists_validates_account_name` — asserts the three rejection cases
(empty, illegal char, reserved `.pubkey` suffix) all return `false`.

### #19 — `hex_decode` can panic on non-ASCII input (low) — resolved

`hex_decode` walked the input string with byte offsets (`&hex[i..i + 2]`)
after only verifying the byte length was even. Multi-byte UTF-8
characters (e.g. `"éé"` — 4 bytes, even length) would pass the length
check, then slice the string mid-codepoint and panic.

Reachable when a backend returns a malformed value through the hex
loader — for example, a compromised 1Password item, a corrupted file
on disk, or a future backend that doesn't sanitize stored bytes.

**Fix** (`src/store.rs`): rewrote the loop to operate on `hex.as_bytes()`
via `chunks_exact(2)`, then validate each chunk with
`std::str::from_utf8` before `u8::from_str_radix`. Non-ASCII bytes
return `Error::InvalidKeypair("hex contains non-ASCII bytes")`; the
function no longer panics on any input.

**Regression tests** (`store.rs` tests module):

- `hex_decode_rejects_non_ascii_input` — feeds `"éé"` (4 bytes, even
  length) and asserts an `InvalidKeypair` error. Panicked against the
  pre-fix code; passes after.
- `hex_decode_rejects_odd_length` — keeps the existing odd-length
  guarantee.
- `hex_decode_roundtrips_ascii` — sanity-checks that normal hex
  decoding still works.

### #40 — `lock()` errors not detected (informational) — resolved

`InMemoryStore`'s `SecretStore` impl was unwrapping the inner mutex
on every operation. A poisoned mutex (panic in another thread while
holding the lock) would crash the process instead of returning a
recoverable error.

**Fix** (`src/store.rs`): the three `Result`-returning operations
(`store`, `load`, `delete`) now map a poison error to
`Error::Backend("in-memory store mutex poisoned")`. `exists` has no
`Result` channel, so it returns `false` on poison — the safer failure
mode for callers that branch on it (matches the "key absent" path
they already handle).

The audit applies cleanly to the in-memory store; the 1Password
store's separate `.lock()` call site is out of scope here since that
backend is being removed (#10). The Linux Secret Service backend
uses async `col.lock().await`, which is a Secret Service collection
relock — different semantics (see #31, #50), tracked separately.

### #38 — `is_available()` functions called inconsistently (informational) — resolved

The auditor's matrix flagged that each platform's `*_available()` shim
was checking a different layer:

| Backend  | Was checking | Real gap |
| -------- | ------------ | -------- |
| macOS    | auth gate only | none — Keychain always present |
| Linux    | store only     | **yes** — Polkit action could be missing while Secret Service is up |
| Windows  | auth gate only | none — Credential Manager always present |

**Fix** (`src/lib.rs`): `gnome_keyring_available` now requires
**both** `SecretServiceStore.is_available()` **and**
`Polkit.is_available()` to return true. Reporting the backend as
"available" when only Secret Service is up was the audit's
documented hazard: callers could commit to the GNOME path on a host
whose Polkit action is missing, then hit the failure at the next
`authenticate()` call instead of at the explicit availability probe.

macOS and Windows checks stay as-is — the auditor's matrix marks
"Keychain access follows device lock, no separate check needed" and
"Credential Manager always available if WinRT works", which matches
the current per-platform checks (auth gate only).

**Why this also touches audit #44:** the underlying Polkit-action
existence check that `Polkit::is_available` performs is the work
described in #44; both findings now share the same code path.

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
