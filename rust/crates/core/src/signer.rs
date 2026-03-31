//! Resolve a signer from a keypair source — file path, Keychain, or 1Password.

use solana_mpp::solana_keychain::MemorySigner;

use crate::{Error, Result};

/// Load a `MemorySigner` from the given source.
///
/// - `keychain:<account>` — load from macOS Keychain (triggers Touch ID)
/// - `gnome-keyring:<account>` — load from GNOME Keyring (triggers polkit)
/// - `windows-hello:<account>` — load from Windows Credential Manager (triggers Windows Hello)
/// - `1password:<account>` — load from 1Password (triggers `op` CLI auth)
/// - anything else — treat as a file path
pub fn load_signer(source: &str) -> Result<MemorySigner> {
    load_signer_with_reason(source, "authorize payment")
}

/// Load a `MemorySigner` with a custom reason string.
pub fn load_signer_with_reason(source: &str, reason: &str) -> Result<MemorySigner> {
    if let Some(account) = source.strip_prefix("keychain:") {
        load_from_keystore_backend("keychain", account, reason)
    } else if let Some(account) = source.strip_prefix("gnome-keyring:") {
        load_from_keystore_backend("gnome-keyring", account, reason)
    } else if let Some(account) = source.strip_prefix("windows-hello:") {
        load_from_keystore_backend("windows-hello", account, reason)
    } else if let Some(account) = source.strip_prefix("1password:") {
        load_from_keystore_backend("1password", account, reason)
    } else {
        load_from_file(source)
    }
}

fn load_from_file(path: &str) -> Result<MemorySigner> {
    let expanded = shellexpand::tilde(path);
    MemorySigner::from_private_key_string(&expanded)
        .map_err(|e| Error::Config(format!("Failed to load keypair from {path}: {e}")))
}

fn load_from_keystore_backend(backend: &str, account: &str, reason: &str) -> Result<MemorySigner> {
    let keystore = match backend {
        #[cfg(target_os = "macos")]
        "keychain" => crate::keystore::Keystore::apple_keychain(),
        #[cfg(not(target_os = "macos"))]
        "keychain" => {
            return Err(Error::Config(
                "Keychain not available on this platform".to_string(),
            ))
        }

        #[cfg(target_os = "linux")]
        "gnome-keyring" => crate::keystore::Keystore::gnome_keyring(),
        #[cfg(not(target_os = "linux"))]
        "gnome-keyring" => {
            return Err(Error::Config(
                "GNOME Keyring not available on this platform".to_string(),
            ))
        }

        #[cfg(target_os = "windows")]
        "windows-hello" => crate::keystore::Keystore::windows_hello(),
        #[cfg(not(target_os = "windows"))]
        "windows-hello" => {
            return Err(Error::Config(
                "Windows Hello not available on this platform".to_string(),
            ))
        }

        "1password" => crate::keystore::Keystore::onepassword(),

        _ => {
            return Err(Error::Config(format!(
                "Unknown keystore backend: {backend}"
            )))
        }
    };

    let bytes = keystore
        .load_keypair(account, reason)
        .map_err(|e| Error::Config(format!("{backend}: {e}")))?;

    MemorySigner::from_bytes(&bytes)
        .map_err(|e| Error::Config(format!("Invalid keypair from {backend}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: We do NOT test keychain:/gnome-keyring:/1password: prefixes here
    // because they trigger interactive auth prompts (Touch ID, op CLI, etc.)
    // that hang in CI/test environments.

    #[test]
    fn load_signer_file_not_found() {
        let result = load_signer("/nonexistent/path/to/keypair.json");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to load keypair"));
    }

    #[test]
    fn load_signer_with_valid_keypair_file() {
        use solana_mpp::solana_keychain::SolanaSigner;

        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let mut keypair_bytes = Vec::with_capacity(64);
        keypair_bytes.extend_from_slice(&signing_key.to_bytes());
        keypair_bytes.extend_from_slice(&verifying_key.to_bytes());

        let temp_dir = tempfile::tempdir().unwrap();
        let key_path = temp_dir.path().join("test-keypair.json");
        let json: Vec<u8> = keypair_bytes;
        std::fs::write(&key_path, serde_json::to_string(&json).unwrap()).unwrap();

        let signer = load_signer(key_path.to_str().unwrap()).unwrap();
        let expected_pubkey = bs58::encode(verifying_key.to_bytes()).into_string();
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
    }

    #[test]
    fn load_signer_invalid_file_content() {
        let temp_dir = tempfile::tempdir().unwrap();
        let key_path = temp_dir.path().join("bad-keypair.json");
        std::fs::write(&key_path, "not valid keypair data").unwrap();

        let result = load_signer(key_path.to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn load_signer_windows_hello_unavailable() {
        #[cfg(not(target_os = "windows"))]
        {
            let result = load_signer("windows-hello:default");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("not available on this platform"));
        }
    }
}
