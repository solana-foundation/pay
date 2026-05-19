//! pay-keystore — pluggable secure storage for Solana keypairs.
//!
//! Separates two concerns:
//! - **AuthGate** — how the user proves identity (Touch ID, Windows Hello, polkit, none)
//! - **SecretStore** — where encrypted bytes live (Keychain, Credential Manager, 1Password, memory)
//!
//! The `Keystore` struct composes them with shared logic (keypair validation, pubkey separation).

pub mod auth;
mod error;
pub mod store;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

pub use auth::{AuthGate, AuthIntent, PaymentLimit};
pub use error::{Error, Result};
pub use store::SecretStore;
pub use zeroize::Zeroizing;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Controls whether the key syncs to cloud storage.
///
/// Audit #5: every supported backend currently stores items as
/// device-only, and the keystore does not yet propagate `SyncMode` into
/// the backend write path. The `CloudSync` variant is commented out
/// until cloud sync is a real, enforced feature — accepting it today
/// would silently mislead callers into believing Pay had honored a sync
/// policy that the backend never sees. Re-add the variant alongside the
/// `kSecAttrSynchronizable` plumbing on macOS (and the equivalent on
/// other backends) when the feature lands.
#[derive(Debug, Clone, Copy, Default)]
pub enum SyncMode {
    /// Key stays on this device only (default).
    #[default]
    ThisDeviceOnly,
    // CloudSync — reserved for future cloud-sync support. Do NOT
    // re-enable without also wiring the policy through `SecretStore` and
    // having each backend declare which modes it can honor.
}

/// Composed keystore: auth gate + secret store + shared logic.
///
/// # Security note
///
/// The auth gate is an **advisory** layer — callers can construct a
/// `Keystore` with [`NoAuth`](auth::NoAuth) paired with any platform
/// store. The real security boundary is the OS credential store itself
/// (Keychain ACLs, DPAPI, Secret Service encryption). The auth gate
/// provides UX-level protection (biometric prompts) but does not prevent
/// programmatic access by code running in the same process.
///
/// ## Threat coverage by backend
///
/// | Threat                                  | macOS Keychain   | Linux Secret Service | Windows Cred. Mgr. |
/// | --------------------------------------- | ---------------- | -------------------- | ------------------ |
/// | Different OS user account               | blocked          | blocked              | blocked            |
/// | Same-user process (e.g. malware)        | **not blocked**  | **not blocked**      | **not blocked**    |
/// | Physical access, device unlocked        | **not blocked**  | **not blocked**      | **not blocked**    |
/// | Physical access, device locked          | blocked          | depends on keyring lock state | blocked   |
///
/// "Blocked" means the OS credential store refuses to release the
/// keypair bytes; "not blocked" means the auth-gate prompt is the only
/// barrier and a co-resident program could load the key without one.
///
/// **macOS** — items use `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`,
/// so the keychain refuses reads while the screen is locked, but any
/// program running as the same user with the screen unlocked can call
/// `SecItemCopyMatching` and skip our [`AuthGate`](auth::AuthGate)
/// entirely. We do not currently set `kSecAttrAccessControl` to bind
/// the item to biometric presence — see `security_report.md` (audit #1)
/// for the rationale.
///
/// **Linux** — Secret Service decrypts items only when the user's
/// keyring is unlocked. "Locked device" coverage therefore depends on
/// the keyring being relocked when the session locks (true under GNOME
/// `gnome-keyring-daemon` with default settings; not guaranteed on
/// every desktop). Same-user processes can talk to Secret Service
/// directly and bypass our Polkit gate.
///
/// **Windows** — Credential Manager binds items to the OS user
/// session; locking the device evicts the secrets from the working
/// set, but any same-user process can call `CredReadW` without
/// invoking Windows Hello.
///
/// **All backends** — physical access to an unlocked device is treated
/// as game-over: a sufficiently determined attacker can drive the UI
/// to trigger our own auth prompts. The auth gate is intended for
/// remote / unattended-process protection, not for shoulder-surfing
/// defense.
pub struct Keystore {
    auth: Box<dyn AuthGate>,
    store: Box<dyn SecretStore>,
    auth_on_write: bool,
    /// Per-account write lock (audit #25). Each logical account is two
    /// backend records (`keypair:<name>` + `pubkey:<name>`); serializing
    /// mutations on the same account in this process prevents two
    /// concurrent imports / deletes from producing a state that no
    /// single successful operation could (e.g. `keypair` from T1 +
    /// `pubkey` from T2). Cross-process concurrency is out of scope —
    /// the backends themselves don't expose transactional primitives.
    account_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Keystore {
    /// Create a keystore from any auth gate and secret store.
    pub fn new(
        auth: impl AuthGate + 'static,
        store: impl SecretStore + 'static,
        auth_on_write: bool,
    ) -> Self {
        Self {
            auth: Box::new(auth),
            store: Box::new(store),
            auth_on_write,
            account_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Return (creating if needed) the per-account write lock.
    fn account_lock(&self, account: &str) -> Arc<Mutex<()>> {
        let mut map = self
            .account_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        Arc::clone(map.entry(account.to_string()).or_default())
    }

    /// In-memory keystore for testing. No auth, no persistence.
    pub fn in_memory() -> Self {
        Self::new(auth::NoAuth, store::InMemoryStore::new(), false)
    }

    /// 1Password via `op` CLI with signout/signin auth cycle.
    pub fn onepassword(account: Option<String>) -> Self {
        Self::new(
            store::OnePasswordAuth::new(account.clone()),
            store::OnePasswordStore::new(account),
            true,
        )
    }

    /// 1Password targeting a specific vault.
    pub fn onepassword_with_vault(vault: impl Into<String>, account: Option<String>) -> Self {
        Self::new(
            store::OnePasswordAuth::new(account.clone()),
            store::OnePasswordStore::with_vault(vault, account),
            true,
        )
    }

    /// Check if 1Password CLI is available.
    pub fn onepassword_available() -> bool {
        store::OnePasswordStore::is_available()
    }

    /// macOS Keychain + Touch ID.
    #[cfg(target_os = "macos")]
    pub fn apple_keychain() -> Self {
        Self::new(macos::TouchId, macos::AppleKeychainStore, true)
    }

    /// Check if Touch ID is available (macOS only).
    #[cfg(target_os = "macos")]
    pub fn apple_touchid_available() -> bool {
        macos::TouchId.is_available()
    }

    /// GNOME Keyring + polkit auth.
    #[cfg(target_os = "linux")]
    pub fn gnome_keyring() -> Self {
        Self::new(linux::Polkit, linux::SecretServiceStore, true)
    }

    /// GNOME Keyring without the polkit gate.
    ///
    /// Used by callers that have already authenticated through a
    /// higher-level policy and want to skip the per-call polkit prompt
    /// (e.g. pay-core's signer resolver when an account doesn't require
    /// per-network auth). Exposed as a dedicated constructor so the
    /// concrete Linux store stays `pub(crate)` (audit #49) — external
    /// crates can no longer bypass the auth coupling by hand-rolling
    /// `Keystore::new(NoAuth, SecretServiceStore, …)`.
    #[cfg(target_os = "linux")]
    pub fn gnome_keyring_no_auth() -> Self {
        Self::new(auth::NoAuth, linux::SecretServiceStore, false)
    }

    /// Check if the Linux GNOME Keyring backend is fully available.
    ///
    /// "Fully available" means both layers the backend needs are usable:
    /// the Secret Service D-Bus collection (the store) **and** the
    /// Polkit auth gate. Reporting "yes" purely on Secret Service alone
    /// would let a caller commit to GNOME Keyring on a host whose Polkit
    /// action is missing, only to fail at the next `authenticate()`
    /// call. macOS and Windows backends only need the auth gate to be
    /// usable (Keychain / Credential Manager are always present on
    /// those platforms); Linux is the one place where both legs can
    /// move independently. (audit #38 / #44)
    #[cfg(target_os = "linux")]
    pub fn gnome_keyring_available() -> bool {
        linux::SecretServiceStore.is_available() && linux::Polkit.is_available()
    }

    /// Windows Credential Manager + Windows Hello.
    #[cfg(target_os = "windows")]
    pub fn windows_hello() -> Self {
        Self::new(
            windows::WindowsHelloAuth,
            windows::WindowsCredentialStore,
            true,
        )
    }

    /// Check if Windows Hello is available.
    #[cfg(target_os = "windows")]
    pub fn windows_hello_available() -> bool {
        AuthGate::is_available(&windows::WindowsHelloAuth)
    }

    // ── Public API ──────────────────────────────────────────────────────

    /// Import a 64-byte keypair (32 secret + 32 public).
    ///
    /// Authenticates with [`AuthIntent::import_account`] (audit #20): the
    /// previous version routed through [`AuthIntent::create_account`],
    /// which is a different Polkit action on Linux (`sh.pay.create-account`
    /// vs `sh.pay.import-account`). Convenience callers that didn't supply
    /// an explicit intent therefore prompted the user with the wrong
    /// approval class for an import.
    pub fn import(&self, account: &str, keypair_bytes: &[u8], _sync: SyncMode) -> Result<()> {
        self.import_with_intent(
            account,
            keypair_bytes,
            _sync,
            &AuthIntent::import_account(account),
        )
    }

    /// Import with a custom auth prompt reason shown to the user.
    pub fn import_with_reason(
        &self,
        account: &str,
        keypair_bytes: &[u8],
        _sync: SyncMode,
        reason: &str,
    ) -> Result<()> {
        self.import_with_intent(
            account,
            keypair_bytes,
            _sync,
            &AuthIntent::from_reason(reason),
        )
    }

    /// Import with a typed auth intent shown to the user where supported.
    pub fn import_with_intent(
        &self,
        account: &str,
        keypair_bytes: &[u8],
        _sync: SyncMode,
        intent: &AuthIntent,
    ) -> Result<()> {
        validate_account_name(account)?;
        // Audit #8: derive the pubkey from the secret seed; reject any
        // caller-supplied bytes that disagree with the derivation. The
        // returned pubkey is what we then commit as the metadata record,
        // so the stored identity always comes from the validated signing
        // key.
        let derived_pubkey = validate_keypair(keypair_bytes)?;

        if self.auth_on_write {
            self.auth.authenticate(intent)?;
        }

        // Audit #25: serialize same-account mutations so an interleaved
        // import / delete from another thread can't produce a state
        // that no single successful operation could (e.g. keypair from
        // T1 + pubkey from T2).
        let lock = self.account_lock(account);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

        // Audit #11: the import is logically a single operation, but it
        // touches two backend records — the keypair and the public-key
        // metadata. If the second write fails after the first has
        // committed, the API would otherwise return Err while leaving
        // private key bytes orphaned in the backend. Roll back the
        // keypair write on a pubkey-write failure so the post-import
        // state matches the returned result.
        self.store.store(&keypair_key(account), keypair_bytes)?;
        if let Err(e) = self
            .store
            .store(&pubkey_key(account), &derived_pubkey)
        {
            let _ = self.store.delete(&keypair_key(account));
            return Err(e);
        }
        Ok(())
    }

    /// Check if a keypair exists for this account.
    pub fn exists(&self, account: &str) -> bool {
        validate_account_name(account).is_ok() && self.store.exists(&keypair_key(account))
    }

    /// Delete a keypair. `reason` is shown in the OS auth prompt (Touch ID, etc.).
    pub fn delete(&self, account: &str, reason: &str) -> Result<()> {
        self.delete_with_intent(account, &AuthIntent::from_reason(reason))
    }

    /// Delete a keypair with a typed auth intent.
    ///
    /// audit #12: do not silently swallow the pubkey-metadata delete.
    /// The previous implementation removed the private keypair record
    /// and treated the `.pubkey` cleanup as best-effort; a failure on
    /// the second leg left the keystore split — `exists()` reported
    /// `false` because the keypair was gone, while `pubkey()` could
    /// still return stale metadata. Propagating the second error makes
    /// the API result honest: callers see exactly the durable state
    /// they got.
    pub fn delete_with_intent(&self, account: &str, intent: &AuthIntent) -> Result<()> {
        validate_account_name(account)?;
        if self.auth_on_write {
            self.auth.authenticate(intent)?;
        }

        // Audit #25: serialize same-account mutations (see import_with_intent).
        let lock = self.account_lock(account);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

        self.store.delete(&keypair_key(account))?;
        self.store.delete(&pubkey_key(account))?;
        Ok(())
    }

    /// Get the 32-byte public key without requiring auth.
    pub fn pubkey(&self, account: &str) -> Result<Vec<u8>> {
        validate_account_name(account)?;
        let pubkey = self.store.load(&pubkey_key(account))?;
        validate_pubkey(&pubkey)?;
        Ok(pubkey.to_vec())
    }

    /// Load the full 64-byte keypair. Triggers auth prompt.
    ///
    /// `reason` is used only as display text. Audit #7: this entry point
    /// is a key-read operation, so the intent is pinned to
    /// [`AuthIntent::use_account`]. Caller-supplied reason text never
    /// upgrades the operation into a privileged variant
    /// (`DeleteAccount`, `AuthorizePayment`, …), which would otherwise
    /// shift the Linux Polkit action away from the account-use action.
    pub fn load_keypair(&self, account: &str, reason: &str) -> Result<Zeroizing<Vec<u8>>> {
        self.load_keypair_with_intent(account, &AuthIntent::use_account(reason))
    }

    /// Load the full 64-byte keypair with a typed auth intent.
    pub fn load_keypair_with_intent(
        &self,
        account: &str,
        intent: &AuthIntent,
    ) -> Result<Zeroizing<Vec<u8>>> {
        validate_account_name(account)?;
        self.auth.authenticate(intent)?;
        let keypair = self.store.load(&keypair_key(account))?;
        validate_keypair(&keypair)?;
        Ok(keypair)
    }

    /// Authenticate without loading anything (for standalone prompts).
    pub fn authenticate(&self, reason: &str) -> Result<()> {
        self.authenticate_intent(&AuthIntent::from_reason(reason))
    }

    /// Authenticate without loading anything using a typed auth intent.
    pub fn authenticate_intent(&self, intent: &AuthIntent) -> Result<()> {
        self.auth.authenticate(intent)
    }

    /// Check if the auth mechanism is available.
    pub fn auth_available(&self) -> bool {
        self.auth.is_available()
    }
}

// ── Shared helpers ──────────────────────────────────────────────────────────

const KEYPAIR_LEN: usize = 64;
const PUBKEY_LEN: usize = 32;
const KEYPAIR_KEY_PREFIX: &str = "keypair:";
const PUBKEY_KEY_PREFIX: &str = "pubkey:";
const RESERVED_PUBKEY_SUFFIX: &str = ".pubkey";

fn validate_account_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidKeypair(
            "account name cannot be empty".to_string(),
        ));
    }
    // audit #16: reject uppercase ASCII letters. The Windows Credential
    // Manager backend keys items by case-insensitive target name, so
    // `Default` and `default` collide there even though they would be
    // distinct accounts on macOS Keychain and Linux Secret Service.
    // Enforcing lowercase-only at the validator gives every backend the
    // same uniqueness contract — the doc comment in this file (and the
    // error message users see) now matches the actual allowed set.
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(Error::InvalidKeypair(format!(
            "account name contains invalid characters: {name:?} (allowed: a-z, 0-9, '.', '_', '-')"
        )));
    }
    if name.ends_with(RESERVED_PUBKEY_SUFFIX) {
        return Err(Error::InvalidKeypair(format!(
            "account name uses reserved suffix: {RESERVED_PUBKEY_SUFFIX}"
        )));
    }
    Ok(())
}

/// Validate a 64-byte Solana / Ed25519 keypair and return the public key
/// derived from its secret seed.
///
/// Solana stores keypairs as `[secret_seed_32 || public_key_32]`. This
/// function does not trust the caller-supplied public-key bytes:
///   1. checks the buffer is exactly 64 bytes;
///   2. interprets bytes `0..32` as the Ed25519 signing seed;
///   3. derives the matching `VerifyingKey` via `ed25519-dalek`;
///   4. rejects the import if the derived public key does not byte-equal
///      the caller-supplied `32..64` half.
///
/// The returned 32 bytes are the *derived* public key. Callers should use
/// this value for the pubkey-metadata record so the stored identity comes
/// from the validated signing key — never from caller-supplied bytes that
/// could disagree with the secret (audit #8).
fn validate_keypair(bytes: &[u8]) -> Result<[u8; PUBKEY_LEN]> {
    if bytes.len() != KEYPAIR_LEN {
        return Err(Error::InvalidKeypair(format!(
            "expected {KEYPAIR_LEN} bytes, got {}",
            bytes.len()
        )));
    }

    let seed: [u8; 32] = bytes[..32]
        .try_into()
        .expect("32 bytes guaranteed by length check");
    let derived = ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .to_bytes();

    let supplied: [u8; 32] = bytes[32..KEYPAIR_LEN]
        .try_into()
        .expect("32 bytes guaranteed by length check");
    if derived != supplied {
        return Err(Error::InvalidKeypair(
            "public key bytes do not match the secret-derived public key".to_string(),
        ));
    }

    Ok(derived)
}

fn validate_pubkey(bytes: &[u8]) -> Result<()> {
    if bytes.len() != PUBKEY_LEN {
        return Err(Error::InvalidKeypair(format!(
            "expected {PUBKEY_LEN} public key bytes, got {}",
            bytes.len()
        )));
    }
    Ok(())
}

fn keypair_key(account: &str) -> String {
    format!("{KEYPAIR_KEY_PREFIX}{account}")
}

fn pubkey_key(account: &str) -> String {
    format!("{PUBKEY_KEY_PREFIX}{account}")
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a real Ed25519 keypair (secret seed + derived public key) for
    /// an all-`seed_byte` secret. Tests need this rather than a hand-rolled
    /// `[0xAA; 32] || [0xBB; 32]` buffer because `validate_keypair` now
    /// rejects pubkey halves that don't derive from the secret (audit #8).
    fn make_keypair(seed_byte: u8) -> Vec<u8> {
        let seed = [seed_byte; 32];
        let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pk = sk.verifying_key().to_bytes();
        let mut out = seed.to_vec();
        out.extend_from_slice(&pk);
        out
    }

    /// 32-byte Ed25519 public key derived from an all-`seed_byte` secret.
    fn pubkey_for(seed_byte: u8) -> Vec<u8> {
        let seed = [seed_byte; 32];
        ed25519_dalek::SigningKey::from_bytes(&seed)
            .verifying_key()
            .to_bytes()
            .to_vec()
    }

    fn test_keypair() -> Vec<u8> {
        make_keypair(0xAA)
    }

    #[test]
    fn in_memory_import_and_exists() {
        let ks = Keystore::in_memory();
        assert!(!ks.exists("test"));
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        assert!(ks.exists("test"));
    }

    #[test]
    fn validate_rejects_uppercase_letters() {
        // audit #16: Windows Credential Manager folds case, so
        // `Default` and `default` would collide there even though
        // they would be distinct on macOS Keychain / Linux Secret
        // Service. Reject uppercase up front so every backend sees
        // the same uniqueness contract.
        assert!(
            matches!(validate_account_name("Default"), Err(Error::InvalidKeypair(_))),
            "uppercase initial letter must be rejected"
        );
        assert!(
            matches!(validate_account_name("FOO"), Err(Error::InvalidKeypair(_))),
            "all-uppercase must be rejected"
        );
        assert!(
            matches!(validate_account_name("MyAccount"), Err(Error::InvalidKeypair(_))),
            "mixed-case must be rejected"
        );
        assert!(
            validate_account_name("default").is_ok(),
            "lowercase must continue to work"
        );
        assert!(
            validate_account_name("alice-1.test_2").is_ok(),
            "lowercase + allowed punctuation must continue to work"
        );
    }

    #[test]
    fn exists_validates_account_name() {
        // audit #26: `exists()` must reject invalid account names
        // (empty, illegal characters, reserved `.pubkey` suffix) before
        // touching the backend. A bypass would let callers probe
        // arbitrary backend keys through what is otherwise a typed API.
        let ks = Keystore::in_memory();
        assert!(!ks.exists(""), "empty name must report false");
        assert!(!ks.exists("bad/name"), "illegal character must report false");
        assert!(
            !ks.exists("victim.pubkey"),
            "reserved .pubkey suffix must report false even if the backend has data"
        );
    }

    #[test]
    fn in_memory_pubkey() {
        let ks = Keystore::in_memory();
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        let pubkey = ks.pubkey("test").unwrap();
        assert_eq!(pubkey, pubkey_for(0xAA));
    }

    #[test]
    fn in_memory_load_keypair() {
        let ks = Keystore::in_memory();
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        let kp = ks.load_keypair("test", "unit test").unwrap();
        assert_eq!(kp.len(), 64);
        assert_eq!(&kp[..32], &[0xAA; 32]);
        assert_eq!(&kp[32..], pubkey_for(0xAA).as_slice());
    }

    #[test]
    fn in_memory_delete() {
        let ks = Keystore::in_memory();
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        assert!(ks.exists("test"));
        ks.delete("test", "test").unwrap();
        assert!(!ks.exists("test"));
    }

    #[test]
    fn in_memory_load_nonexistent() {
        let ks = Keystore::in_memory();
        assert!(ks.load_keypair("missing", "test").is_err());
    }

    #[test]
    fn in_memory_pubkey_nonexistent() {
        let ks = Keystore::in_memory();
        assert!(ks.pubkey("missing").is_err());
    }

    #[test]
    fn validate_keypair_wrong_size() {
        let ks = Keystore::in_memory();
        let result = ks.import("test", &[0u8; 32], SyncMode::ThisDeviceOnly);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("expected 64 bytes")
        );
    }

    #[test]
    fn validate_keypair_empty() {
        let ks = Keystore::in_memory();
        assert!(ks.import("test", &[], SyncMode::ThisDeviceOnly).is_err());
    }

    #[test]
    fn in_memory_multiple_accounts() {
        let ks = Keystore::in_memory();
        ks.import("acct1", &make_keypair(0x11), SyncMode::ThisDeviceOnly)
            .unwrap();
        ks.import("acct2", &make_keypair(0x33), SyncMode::ThisDeviceOnly)
            .unwrap();

        assert_eq!(ks.pubkey("acct1").unwrap(), pubkey_for(0x11));
        assert_eq!(ks.pubkey("acct2").unwrap(), pubkey_for(0x33));

        ks.delete("acct1", "test").unwrap();
        assert!(!ks.exists("acct1"));
        assert!(ks.exists("acct2"));
    }

    #[test]
    fn in_memory_overwrite() {
        let ks = Keystore::in_memory();
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();

        ks.import("test", &make_keypair(0xCC), SyncMode::ThisDeviceOnly)
            .unwrap();

        assert_eq!(ks.pubkey("test").unwrap(), pubkey_for(0xCC));
    }

    #[test]
    fn auth_on_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingAuth(Arc<AtomicU32>);
        impl AuthGate for CountingAuth {
            fn authenticate(&self, _intent: &AuthIntent) -> Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }

        let counter = Arc::new(AtomicU32::new(0));
        let ks = Keystore {
            auth: Box::new(CountingAuth(counter.clone())),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: true,
            account_locks: Mutex::new(HashMap::new()),
        };

        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1); // import calls auth

        ks.load_keypair("test", "test").unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2); // load_keypair calls auth

        ks.delete("test", "test").unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 3); // delete calls auth
    }

    #[test]
    fn no_auth_on_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingAuth(Arc<AtomicU32>);
        impl AuthGate for CountingAuth {
            fn authenticate(&self, _intent: &AuthIntent) -> Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }

        let counter = Arc::new(AtomicU32::new(0));
        let ks = Keystore {
            auth: Box::new(CountingAuth(counter.clone())),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        };

        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0); // import does NOT call auth

        ks.load_keypair("test", "test").unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1); // load_keypair calls auth

        ks.delete("test", "test").unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1); // delete does NOT call auth
    }

    #[test]
    fn no_auth_is_always_available() {
        let ks = Keystore::in_memory();
        assert!(ks.auth_available());
    }

    #[test]
    fn authenticate_standalone() {
        let ks = Keystore::in_memory();
        ks.authenticate("test reason").unwrap();
    }

    #[test]
    fn delete_nonexistent_succeeds() {
        let ks = Keystore::in_memory();
        ks.delete("nonexistent", "test").unwrap();
    }

    #[test]
    fn sync_mode_default_is_this_device_only() {
        assert!(matches!(SyncMode::default(), SyncMode::ThisDeviceOnly));
    }

    #[test]
    fn keypair_key_naming() {
        assert_eq!(keypair_key("default"), "keypair:default");
        assert_eq!(pubkey_key("default"), "pubkey:default");
    }

    #[test]
    fn reserved_pubkey_suffix_is_rejected() {
        let ks = Keystore::in_memory();
        let result = ks.import("victim.pubkey", &test_keypair(), SyncMode::ThisDeviceOnly);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("reserved suffix"));
        assert!(!ks.exists("victim.pubkey"));
    }

    #[test]
    fn pubkey_rejects_private_keypair_sized_value() {
        let ks = Keystore::in_memory();
        ks.store
            .store(&pubkey_key("victim"), &test_keypair())
            .unwrap();

        let result = ks.pubkey("victim");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("expected 32 public key bytes")
        );
    }

    #[test]
    fn pubkey_rejects_truncated_backend_record() {
        // audit #34: load APIs must not trust whatever length the
        // backend returns. A truncated pubkey record (e.g. 16 bytes
        // from a tampered or corrupted store) must be rejected, not
        // returned to the caller as if it were a valid public key.
        let ks = Keystore::in_memory();
        ks.store
            .store(&pubkey_key("victim"), &[0u8; 16])
            .unwrap();
        let result = ks.pubkey("victim");
        assert!(matches!(result, Err(Error::InvalidKeypair(_))));
    }

    #[test]
    fn load_keypair_rejects_malformed_backend_record() {
        // audit #34: load_keypair_with_intent runs validate_keypair on
        // the bytes returned by the SecretStore. A backend record that
        // is the wrong length (or fails the derive-then-compare check)
        // must surface as an InvalidKeypair error, never reach the
        // caller as a "valid" 64-byte slice.
        let ks = Keystore {
            auth: Box::new(auth::NoAuth),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        };
        // Plant a wrong-length record directly under the typed key.
        ks.store
            .store(&keypair_key("victim"), &[0u8; 48])
            .unwrap();
        let result = ks.load_keypair("victim", "unit test");
        assert!(matches!(result, Err(Error::InvalidKeypair(_))));

        // Same with a 64-byte buffer whose halves disagree — caught by
        // the derive-then-compare path in validate_keypair.
        ks.store
            .store(&keypair_key("victim2"), &[0xAA; 64])
            .unwrap();
        let result = ks.load_keypair("victim2", "unit test");
        assert!(matches!(result, Err(Error::InvalidKeypair(_))));
    }

    #[test]
    fn typed_storage_keys_do_not_alias_valid_account_names() {
        let ks = Keystore::in_memory();
        ks.import("victim", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();

        assert!(ks.exists("victim"));
        assert!(!ks.exists("keypair:victim"));
        assert!(!ks.exists("pubkey:victim"));
    }

    /// End-to-end regression for audit finding #2.
    ///
    /// Original attack: when account names allowed `.` and the pubkey
    /// metadata key was `format!("{account}.pubkey")`, importing an account
    /// named `victim.pubkey` placed the attacker's 64-byte keypair under the
    /// same storage key that `pubkey("victim")` later loaded. Because
    /// `pubkey()` did not enforce a 32-byte size, an unauthenticated caller
    /// could retrieve raw secret-key bytes.
    ///
    /// Mitigations exercised here (all three must hold to block the attack):
    ///   1. typed storage prefixes (`keypair:` / `pubkey:`) prevent the
    ///      attacker's storage key from aliasing the legitimate pubkey key;
    ///   2. `validate_account_name` rejects the reserved `.pubkey` suffix
    ///      (case-insensitive) before any storage write happens;
    ///   3. `pubkey()` validates that the loaded value is exactly 32 bytes.
    #[test]
    fn audit_2_pubkey_collision_attack_is_blocked() {
        let ks = Keystore::in_memory();

        let victim_keypair = make_keypair(0xAA);
        ks.import("victim", &victim_keypair, SyncMode::ThisDeviceOnly)
            .unwrap();

        let attacker_keypair = make_keypair(0xCC);

        let attempt = ks.import("victim.pubkey", &attacker_keypair, SyncMode::ThisDeviceOnly);
        assert!(
            attempt.is_err(),
            "import of `victim.pubkey` must be rejected"
        );
        assert!(
            attempt
                .unwrap_err()
                .to_string()
                .contains("reserved suffix"),
            "rejection must cite the reserved suffix",
        );
        assert!(!ks.exists("victim.pubkey"));

        let leaked = ks.pubkey("victim").expect("legitimate pubkey still readable");
        assert_eq!(leaked, pubkey_for(0xAA), "must return the legitimate pubkey");
        assert_ne!(
            leaked,
            attacker_keypair[..32].to_vec(),
            "must never leak any byte from the attacker's secret half",
        );
        assert_ne!(
            leaked,
            attacker_keypair[32..].to_vec(),
            "must never return the attacker's public half either",
        );

        for variant in ["victim.PUBKEY", "victim.Pubkey", "VICTIM.pubkey"] {
            assert!(
                ks.import(variant, &attacker_keypair, SyncMode::ThisDeviceOnly)
                    .is_err(),
                "case variant {variant:?} of the reserved suffix must also be rejected",
            );
        }
    }

    #[test]
    fn hex_roundtrip() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let hex = store::hex_encode(&data);
        assert_eq!(&*hex, "deadbeef");
        let decoded = store::hex_decode(&hex).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn hex_decode_odd_length() {
        assert!(store::hex_decode("abc").is_err());
    }

    #[test]
    fn hex_decode_invalid_chars() {
        assert!(store::hex_decode("zzzz").is_err());
    }

    // ── Auth denial tests ───────────────────────────────────────────────

    struct DenyAuth;
    impl AuthGate for DenyAuth {
        fn authenticate(&self, _intent: &AuthIntent) -> Result<()> {
            Err(Error::AuthDenied("denied by test".to_string()))
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    #[test]
    fn import_denied_when_auth_on_write() {
        let ks = Keystore {
            auth: Box::new(DenyAuth),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: true,
            account_locks: Mutex::new(HashMap::new()),
        };
        let result = ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("denied"));
        // Nothing should be stored
        assert!(!ks.exists("test"));
    }

    #[test]
    fn import_succeeds_without_auth_when_auth_on_write_false() {
        let ks = Keystore {
            auth: Box::new(DenyAuth),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        };
        // DenyAuth would reject, but auth_on_write=false skips it for import
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        assert!(ks.exists("test"));
    }

    #[test]
    fn load_keypair_denied() {
        let ks = Keystore {
            auth: Box::new(DenyAuth),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        };
        // Import works (no auth on write)
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        // But loading requires auth — should be denied
        let result = ks.load_keypair("test", "test reason");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("denied"));
    }

    #[test]
    fn delete_denied_when_auth_on_write() {
        let ks = Keystore {
            auth: Box::new(DenyAuth),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: true,
            account_locks: Mutex::new(HashMap::new()),
        };
        // Manually store without going through import (which would also be denied)
        ks.store
            .store(&keypair_key("test"), &test_keypair())
            .unwrap();
        ks.store
            .store(&pubkey_key("test"), &pubkey_for(0xAA))
            .unwrap();

        let result = ks.delete("test", "test");
        assert!(result.is_err());
        // Key should still exist
        assert!(ks.exists("test"));
    }

    #[test]
    fn pubkey_does_not_require_auth() {
        let ks = Keystore {
            auth: Box::new(DenyAuth),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        };
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        // pubkey should work even with DenyAuth — no auth required for pubkey
        let pk = ks.pubkey("test").unwrap();
        assert_eq!(pk, pubkey_for(0xAA));
    }

    // ── Audit #7: load_keypair intent classification ────────────────────

    /// AuthGate that records the most recent intent it was asked to
    /// authenticate. Used to assert that key-read APIs hand the gate the
    /// right typed variant regardless of caller-supplied reason text.
    #[derive(Clone)]
    struct RecordingAuth {
        captured: std::sync::Arc<std::sync::Mutex<Option<AuthIntent>>>,
    }

    impl RecordingAuth {
        fn new() -> Self {
            Self {
                captured: std::sync::Arc::new(std::sync::Mutex::new(None)),
            }
        }

        fn captured(&self) -> AuthIntent {
            self.captured
                .lock()
                .unwrap()
                .clone()
                .expect("authenticate() must have been called")
        }
    }

    impl AuthGate for RecordingAuth {
        fn authenticate(&self, intent: &AuthIntent) -> Result<()> {
            *self.captured.lock().unwrap() = Some(intent.clone());
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    /// Store mock that fails on the Nth delete (counted from the
    /// first call). Lets us simulate "keypair delete works, pubkey
    /// delete fails" — the audit #12 scenario.
    struct FailOnNthDeleteStore {
        inner: store::InMemoryStore,
        deletes: std::sync::atomic::AtomicU32,
        fail_on_nth: u32,
    }

    impl FailOnNthDeleteStore {
        fn new(fail_on_nth: u32) -> Self {
            Self {
                inner: store::InMemoryStore::new(),
                deletes: std::sync::atomic::AtomicU32::new(0),
                fail_on_nth,
            }
        }
    }

    impl SecretStore for FailOnNthDeleteStore {
        fn store(&self, key: &str, data: &[u8]) -> Result<()> {
            self.inner.store(key, data)
        }
        fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
            self.inner.load(key)
        }
        fn exists(&self, key: &str) -> bool {
            self.inner.exists(key)
        }
        fn delete(&self, key: &str) -> Result<()> {
            let nth = self
                .deletes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if nth == self.fail_on_nth {
                return Err(Error::Backend(
                    "simulated delete failure on Nth call".to_string(),
                ));
            }
            self.inner.delete(key)
        }
    }

    #[test]
    fn concurrent_imports_leave_records_consistent() {
        // audit #25: two threads importing different keypair bytes
        // under the same account name must not produce a split state
        // (e.g. keypair from one thread + pubkey from another). The
        // per-account write lock serializes the two-store sequence so
        // the final state is one consistent (keypair, pubkey) pair.
        use std::sync::Arc;

        let ks = Arc::new(Keystore {
            auth: Box::new(auth::NoAuth),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        });

        let kp_a = make_keypair(0xAA);
        let kp_b = make_keypair(0xBB);
        let pk_a = pubkey_for(0xAA);
        let pk_b = pubkey_for(0xBB);

        // Spawn enough rounds that an unlocked implementation would
        // almost certainly produce at least one interleaved write.
        for _ in 0..50 {
            let ks_a = Arc::clone(&ks);
            let ks_b = Arc::clone(&ks);
            let kp_a_t = kp_a.clone();
            let kp_b_t = kp_b.clone();
            let h1 = std::thread::spawn(move || {
                let _ = ks_a.import("alice", &kp_a_t, SyncMode::ThisDeviceOnly);
            });
            let h2 = std::thread::spawn(move || {
                let _ = ks_b.import("alice", &kp_b_t, SyncMode::ThisDeviceOnly);
            });
            h1.join().unwrap();
            h2.join().unwrap();

            // Whatever won, the pubkey metadata must match the
            // keypair that's currently on disk — never the opposite.
            let keypair = ks
                .store
                .load(&keypair_key("alice"))
                .expect("a winning import must leave a keypair record");
            let pubkey = ks
                .store
                .load(&pubkey_key("alice"))
                .expect("a winning import must leave a pubkey record");
            let winning = if keypair.as_slice() == kp_a.as_slice() {
                &pk_a
            } else {
                &pk_b
            };
            assert_eq!(
                pubkey.as_slice(),
                winning.as_slice(),
                "keypair and pubkey records desynchronized — audit #25 lock missing"
            );
        }
    }

    #[test]
    fn delete_surfaces_pubkey_record_failure() {
        // audit #12: the second delete (pubkey metadata) used to be
        // discarded via `let _ = ...`. A failure there left the
        // keystore split — keypair gone, pubkey record stale, API
        // returned Ok(()). Propagate the error so callers see the
        // real durable state.
        let ks = Keystore {
            auth: Box::new(auth::NoAuth),
            store: Box::new(FailOnNthDeleteStore::new(1)),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        };
        ks.import("victim", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        let result = ks.delete("victim", "unit test");
        assert!(
            result.is_err(),
            "pubkey-delete failure must surface to the caller, not be swallowed",
        );
    }

    #[test]
    fn import_uses_import_account_intent_not_create_account() {
        // Audit #20: the convenience `import()` API used to authenticate
        // with `AuthIntent::create_account`, which on Linux maps to
        // `sh.pay.create-account` rather than the import-specific
        // `sh.pay.import-account`. Caller without an explicit intent
        // therefore prompted with the wrong approval class.
        let recorder = RecordingAuth::new();
        let ks = Keystore {
            auth: Box::new(recorder.clone()),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: true,
            account_locks: Mutex::new(HashMap::new()),
        };
        ks.import("victim", &test_keypair(), SyncMode::ThisDeviceOnly)
            .expect("import should succeed under RecordingAuth");
        let captured = recorder.captured();
        assert!(
            matches!(captured, AuthIntent::ImportAccount(_)),
            "expected ImportAccount, got {captured:?}",
        );
    }

    #[test]
    fn load_keypair_does_not_inherit_privileged_intent_from_reason() {
        // Audit #7: `load_keypair` is a key-read operation. Privileged
        // operation classes (DeleteAccount / AuthorizePayment / …) must
        // never be selected from caller-supplied reason text because they
        // also change the Linux Polkit action.
        let recorder = RecordingAuth::new();
        let ks = Keystore {
            auth: Box::new(recorder.clone()),
            store: Box::new(store::InMemoryStore::new()),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        };
        ks.import("victim", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();

        // The auditor's exact example: delete-shaped prose used as a
        // load_keypair reason. The captured intent must be UseAccount,
        // not DeleteAccount.
        ks.load_keypair("victim", "delete the \"victim\" payment account")
            .unwrap();
        let captured = recorder.captured();
        assert!(
            matches!(captured, AuthIntent::UseAccount(_)),
            "expected UseAccount, got {captured:?}",
        );

        // Payment-shaped prose must also be classified as UseAccount —
        // otherwise the Linux Polkit action would shift to a payment
        // bucket for a read operation.
        ks.load_keypair(
            "victim",
            "authorize payment of $0.0001 for loading the victim keypair",
        )
        .unwrap();
        let captured = recorder.captured();
        assert!(
            matches!(captured, AuthIntent::UseAccount(_)),
            "expected UseAccount, got {captured:?}",
        );
    }

    // ── Audit #8: keypair pubkey/secret consistency ─────────────────────

    #[test]
    fn import_rejects_mismatched_pubkey_bytes() {
        // 0xAA...0xBB is a length-valid 64-byte buffer where the second
        // half is NOT the Ed25519 public key derived from the first half.
        // The audit (#8) calls out the case where a caller can supply
        // unrelated public-key bytes alongside a real signing seed. After
        // this fix, validate_keypair must reject the import.
        let ks = Keystore::in_memory();
        let mut keypair = vec![0xAAu8; 32];
        keypair.extend_from_slice(&[0xBBu8; 32]);

        let result = ks.import("victim", &keypair, SyncMode::ThisDeviceOnly);
        assert!(
            result.is_err(),
            "import must reject a keypair whose public-key half does not derive from the secret",
        );
        assert!(!ks.exists("victim"));
    }

    // ── Audit #11: import atomicity ─────────────────────────────────────

    /// SecretStore that succeeds on the first `store()` call and fails on
    /// every subsequent one. Used to simulate the split-write hazard the
    /// audit describes: the keypair record lands, but the follow-up pubkey
    /// write fails partway through.
    struct FailOnSecondStore {
        inner: store::InMemoryStore,
        writes: std::sync::atomic::AtomicU32,
    }

    impl FailOnSecondStore {
        fn new() -> Self {
            Self {
                inner: store::InMemoryStore::new(),
                writes: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }

    impl SecretStore for FailOnSecondStore {
        fn store(&self, key: &str, data: &[u8]) -> Result<()> {
            let nth = self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if nth >= 1 {
                return Err(Error::Backend(
                    "simulated backend failure on second write".to_string(),
                ));
            }
            self.inner.store(key, data)
        }

        fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
            self.inner.load(key)
        }

        fn exists(&self, key: &str) -> bool {
            self.inner.exists(key)
        }

        fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn import_rolls_back_keypair_when_pubkey_write_fails() {
        // Simulate a backend that commits the keypair record but then
        // errors on the pubkey write. The audit (#11) calls out the case
        // where `import_with_intent` returns Err while the keypair has
        // already been persisted, leaving orphaned private key material.
        let ks = Keystore {
            auth: Box::new(auth::NoAuth),
            store: Box::new(FailOnSecondStore::new()),
            auth_on_write: false,
            account_locks: Mutex::new(HashMap::new()),
        };

        let result = ks.import("victim", &test_keypair(), SyncMode::ThisDeviceOnly);
        assert!(result.is_err(), "import must surface the backend failure");

        // After the failed import, no keypair record may remain. If this
        // assertion fails, the API returned Err but private key bytes are
        // still sitting in the backend — the exact mismatch the audit
        // describes.
        assert!(
            !ks.store.exists(&keypair_key("victim")),
            "keypair record must be rolled back when the pubkey write fails",
        );
    }

    // ── Full lifecycle test ─────────────────────────────────────────────

    #[test]
    fn full_lifecycle_import_read_delete() {
        let ks = Keystore::in_memory();

        // Realistic Ed25519 keypair: all-0x42 seed plus its derived pubkey.
        let secret = [0x42u8; 32];
        let public = pubkey_for(0x42);
        let keypair = make_keypair(0x42);

        // Import
        ks.import("alice", &keypair, SyncMode::ThisDeviceOnly)
            .unwrap();
        assert!(ks.exists("alice"));
        assert!(!ks.exists("bob"));

        // Read pubkey (no auth)
        assert_eq!(ks.pubkey("alice").unwrap(), public);

        // Load full keypair (auth required — NoAuth passes)
        let loaded = ks.load_keypair("alice", "test").unwrap();
        assert_eq!(&loaded[..32], &secret);
        assert_eq!(&loaded[32..], public.as_slice());

        // Delete
        ks.delete("alice", "test").unwrap();
        assert!(!ks.exists("alice"));
        assert!(ks.pubkey("alice").is_err());
        assert!(ks.load_keypair("alice", "test").is_err());
    }
}
