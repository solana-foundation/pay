//! pay-keystore — pluggable secure storage for Solana keypairs.
//!
//! Backends:
//! - `apple_keychain` — macOS Keychain + Touch ID (macOS only)
//! - `onepassword` — 1Password CLI integration (cross-platform)

pub mod backends;
mod error;

pub use error::{Error, Result};

/// Controls whether the key syncs to cloud storage.
#[derive(Debug, Clone, Copy, Default)]
pub enum SyncMode {
    /// Key stays on this device only (default).
    #[default]
    ThisDeviceOnly,
    /// Key syncs to cloud (iCloud Keychain, 1Password, etc.).
    CloudSync,
}

/// Common interface for all keystore backends.
pub trait KeystoreBackend {
    /// Import a keypair (64 bytes: 32 secret + 32 public).
    fn import(&self, account: &str, keypair_bytes: &[u8], sync: SyncMode) -> Result<()>;

    /// Check if a keypair exists.
    fn exists(&self, account: &str) -> bool;

    /// Delete a keypair.
    fn delete(&self, account: &str) -> Result<()>;

    /// Get the public key (32 bytes) without requiring auth.
    fn pubkey(&self, account: &str) -> Result<Vec<u8>>;

    /// Load the full keypair (64 bytes). May trigger auth (Touch ID, password, etc.).
    fn load_keypair(&self, account: &str, reason: &str) -> Result<Vec<u8>>;
}
