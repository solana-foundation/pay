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

(Rows added as we work through findings.)

---

## Per-finding notes

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
