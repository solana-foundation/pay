//! Resolve a signer from a keypair source — file path, Keychain, or 1Password.

use solana_mpp::solana_keychain::MemorySigner;

use crate::{Error, Result};

/// Load a `MemorySigner` from the given source.
///
/// - `keychain:<account>` — load from macOS Keychain (triggers Touch ID)
/// - `1password:<account>` — load from 1Password (triggers `op` CLI auth)
/// - anything else — treat as a file path
pub fn load_signer(source: &str) -> Result<MemorySigner> {
    load_signer_with_reason(source, "authorize payment")
}

/// Load a `MemorySigner` with a custom reason string.
///
/// For Keychain sources, the reason is shown in the Touch ID prompt.
/// For 1Password and file-based keypairs, the reason is ignored.
pub fn load_signer_with_reason(source: &str, reason: &str) -> Result<MemorySigner> {
    if let Some(account) = source.strip_prefix("keychain:") {
        load_from_keychain(account, reason)
    } else if let Some(account) = source.strip_prefix("1password:") {
        load_from_1password(account, reason)
    } else {
        load_from_file(source)
    }
}

fn load_from_file(path: &str) -> Result<MemorySigner> {
    let expanded = shellexpand::tilde(path);
    MemorySigner::from_private_key_string(&expanded)
        .map_err(|e| Error::Config(format!("Failed to load keypair from {path}: {e}")))
}

#[cfg(target_os = "macos")]
fn load_from_keychain(account: &str, reason: &str) -> Result<MemorySigner> {
    use crate::keystore::{AppleKeychain, KeystoreBackend};

    let bytes = AppleKeychain
        .load_keypair(account, reason)
        .map_err(|e| Error::Config(format!("Keychain: {e}")))?;

    MemorySigner::from_bytes(&bytes)
        .map_err(|e| Error::Config(format!("Invalid keypair from Keychain: {e}")))
}

#[cfg(not(target_os = "macos"))]
fn load_from_keychain(_account: &str, _reason: &str) -> Result<MemorySigner> {
    Err(Error::Config(
        "Keychain not available on this platform".to_string(),
    ))
}

fn load_from_1password(account: &str, reason: &str) -> Result<MemorySigner> {
    use crate::keystore::{KeystoreBackend, OnePassword};

    let backend = OnePassword::new();
    let bytes = backend
        .load_keypair(account, reason)
        .map_err(|e| Error::Config(format!("1Password: {e}")))?;

    MemorySigner::from_bytes(&bytes)
        .map_err(|e| Error::Config(format!("Invalid keypair from 1Password: {e}")))
}
