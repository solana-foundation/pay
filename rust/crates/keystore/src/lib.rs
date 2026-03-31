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

pub use auth::AuthGate;
pub use error::{Error, Result};
pub use store::SecretStore;
pub use zeroize::Zeroizing;

/// Controls whether the key syncs to cloud storage.
#[derive(Debug, Clone, Copy, Default)]
pub enum SyncMode {
    /// Key stays on this device only (default).
    #[default]
    ThisDeviceOnly,
    /// Key syncs to cloud (iCloud Keychain, 1Password, etc.).
    CloudSync,
}

/// Composed keystore: auth gate + secret store + shared logic.
pub struct Keystore {
    auth: Box<dyn AuthGate>,
    store: Box<dyn SecretStore>,
    auth_on_write: bool,
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
        }
    }

    /// In-memory keystore for testing. No auth, no persistence.
    pub fn in_memory() -> Self {
        Self::new(auth::NoAuth, store::InMemoryStore::new(), false)
    }

    /// 1Password via `op` CLI. Auth handled by `op` internally.
    pub fn onepassword() -> Self {
        Self::new(auth::NoAuth, store::OnePasswordStore::new(), false)
    }

    /// 1Password targeting a specific vault.
    pub fn onepassword_with_vault(vault: impl Into<String>) -> Self {
        Self::new(
            auth::NoAuth,
            store::OnePasswordStore::with_vault(vault),
            false,
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

    /// Check if GNOME Secret Service is available (Linux only).
    #[cfg(target_os = "linux")]
    pub fn gnome_keyring_available() -> bool {
        linux::SecretServiceStore::is_available()
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
        windows::WindowsHelloAuth::is_available()
    }

    // ── Public API ──────────────────────────────────────────────────────

    /// Import a 64-byte keypair (32 secret + 32 public).
    pub fn import(&self, account: &str, keypair_bytes: &[u8], _sync: SyncMode) -> Result<()> {
        validate_keypair(keypair_bytes)?;

        if self.auth_on_write {
            self.auth.authenticate("store keypair")?;
        }

        self.store.store(&keypair_key(account), keypair_bytes)?;
        self.store
            .store(&pubkey_key(account), &keypair_bytes[32..64])?;
        Ok(())
    }

    /// Check if a keypair exists for this account.
    pub fn exists(&self, account: &str) -> bool {
        self.store.exists(&keypair_key(account))
    }

    /// Delete a keypair.
    pub fn delete(&self, account: &str) -> Result<()> {
        if self.auth_on_write {
            self.auth.authenticate("delete keypair")?;
        }

        self.store.delete(&keypair_key(account))?;
        let _ = self.store.delete(&pubkey_key(account));
        Ok(())
    }

    /// Get the 32-byte public key without requiring auth.
    pub fn pubkey(&self, account: &str) -> Result<Vec<u8>> {
        self.store.load(&pubkey_key(account)).map(|z| z.to_vec())
    }

    /// Load the full 64-byte keypair. Triggers auth prompt.
    pub fn load_keypair(&self, account: &str, reason: &str) -> Result<Zeroizing<Vec<u8>>> {
        self.auth.authenticate(reason)?;
        self.store.load(&keypair_key(account))
    }

    /// Authenticate without loading anything (for standalone prompts).
    pub fn authenticate(&self, reason: &str) -> Result<()> {
        self.auth.authenticate(reason)
    }

    /// Check if the auth mechanism is available.
    pub fn auth_available(&self) -> bool {
        self.auth.is_available()
    }
}

// ── Shared helpers ──────────────────────────────────────────────────────────

fn validate_keypair(bytes: &[u8]) -> Result<()> {
    if bytes.len() != 64 {
        return Err(Error::InvalidKeypair(format!(
            "expected 64 bytes, got {}",
            bytes.len()
        )));
    }
    Ok(())
}

fn keypair_key(account: &str) -> String {
    account.to_string()
}

fn pubkey_key(account: &str) -> String {
    format!("{account}.pubkey")
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keypair() -> Vec<u8> {
        let mut bytes = vec![0xAA; 32];
        bytes.extend_from_slice(&[0xBB; 32]);
        bytes
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
    fn in_memory_pubkey() {
        let ks = Keystore::in_memory();
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        let pubkey = ks.pubkey("test").unwrap();
        assert_eq!(pubkey, vec![0xBB; 32]);
    }

    #[test]
    fn in_memory_load_keypair() {
        let ks = Keystore::in_memory();
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        let kp = ks.load_keypair("test", "unit test").unwrap();
        assert_eq!(kp.len(), 64);
        assert_eq!(&kp[..32], &[0xAA; 32]);
        assert_eq!(&kp[32..], &[0xBB; 32]);
    }

    #[test]
    fn in_memory_delete() {
        let ks = Keystore::in_memory();
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        assert!(ks.exists("test"));
        ks.delete("test").unwrap();
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
        let mut kp1 = vec![0x11; 32];
        kp1.extend_from_slice(&[0x22; 32]);
        let mut kp2 = vec![0x33; 32];
        kp2.extend_from_slice(&[0x44; 32]);

        ks.import("acct1", &kp1, SyncMode::ThisDeviceOnly).unwrap();
        ks.import("acct2", &kp2, SyncMode::ThisDeviceOnly).unwrap();

        assert_eq!(ks.pubkey("acct1").unwrap(), vec![0x22; 32]);
        assert_eq!(ks.pubkey("acct2").unwrap(), vec![0x44; 32]);

        ks.delete("acct1").unwrap();
        assert!(!ks.exists("acct1"));
        assert!(ks.exists("acct2"));
    }

    #[test]
    fn in_memory_overwrite() {
        let ks = Keystore::in_memory();
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();

        let mut kp2 = vec![0xCC; 32];
        kp2.extend_from_slice(&[0xDD; 32]);
        ks.import("test", &kp2, SyncMode::ThisDeviceOnly).unwrap();

        assert_eq!(ks.pubkey("test").unwrap(), vec![0xDD; 32]);
    }

    #[test]
    fn auth_on_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingAuth(Arc<AtomicU32>);
        impl AuthGate for CountingAuth {
            fn authenticate(&self, _reason: &str) -> Result<()> {
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
        };

        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1); // import calls auth

        ks.load_keypair("test", "test").unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2); // load_keypair calls auth

        ks.delete("test").unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 3); // delete calls auth
    }

    #[test]
    fn no_auth_on_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingAuth(Arc<AtomicU32>);
        impl AuthGate for CountingAuth {
            fn authenticate(&self, _reason: &str) -> Result<()> {
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
        };

        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0); // import does NOT call auth

        ks.load_keypair("test", "test").unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1); // load_keypair calls auth

        ks.delete("test").unwrap();
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
        ks.delete("nonexistent").unwrap();
    }

    #[test]
    fn sync_mode_default_is_this_device_only() {
        assert!(matches!(SyncMode::default(), SyncMode::ThisDeviceOnly));
    }

    #[test]
    fn keypair_key_naming() {
        assert_eq!(keypair_key("default"), "default");
        assert_eq!(pubkey_key("default"), "default.pubkey");
    }

    #[test]
    fn hex_roundtrip() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let hex = store::hex_encode(&data);
        assert_eq!(hex, "deadbeef");
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
        fn authenticate(&self, _reason: &str) -> Result<()> {
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
        };
        // Manually store without going through import (which would also be denied)
        ks.store.store("test", &test_keypair()).unwrap();
        ks.store.store("test.pubkey", &[0xBB; 32]).unwrap();

        let result = ks.delete("test");
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
        };
        ks.import("test", &test_keypair(), SyncMode::ThisDeviceOnly)
            .unwrap();
        // pubkey should work even with DenyAuth — no auth required for pubkey
        let pk = ks.pubkey("test").unwrap();
        assert_eq!(pk, vec![0xBB; 32]);
    }

    // ── Full lifecycle test ─────────────────────────────────────────────

    #[test]
    fn full_lifecycle_import_read_delete() {
        let ks = Keystore::in_memory();

        // Generate a realistic keypair
        let secret = [0x42u8; 32];
        let public = [0x7Fu8; 32];
        let mut keypair = Vec::new();
        keypair.extend_from_slice(&secret);
        keypair.extend_from_slice(&public);

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
        assert_eq!(&loaded[32..], &public);

        // Delete
        ks.delete("alice").unwrap();
        assert!(!ks.exists("alice"));
        assert!(ks.pubkey("alice").is_err());
        assert!(ks.load_keypair("alice", "test").is_err());
    }
}
