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
- **wontfix** — accepted risk or out of scope (with rationale).
- **open** — not yet triaged.

## Status table

| #   | Severity      | Title                                                                         | Status   |
| --- | ------------- | ----------------------------------------------------------------------------- | -------- |
| 52  | high          | Use of `/tmp` is unsafe                                                       | resolved |
| 28  | high          | macOS helper uses the current directory when `HOME` is empty                  | resolved |

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
