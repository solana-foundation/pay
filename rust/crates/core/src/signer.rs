//! Resolve a signer from a keypair source — file path, Keychain, or 1Password.

use solana_mpp::solana_keychain::MemorySigner;

use crate::accounts::{
    Account, AccountChoice, AccountsStore, Keystore, ResolvedEphemeral,
    load_or_create_ephemeral_for_network, load_or_create_ephemeral_for_network_as,
    resolve_account_for_network,
};
use crate::{Error, Result};

/// Load a `MemorySigner` from the given source.
///
/// - `keychain:<account>` — load from macOS Keychain (triggers Touch ID)
/// - `gnome-keyring:<account>` — load from GNOME Keyring (triggers polkit)
/// - `windows-hello:<account>` — load from Windows Credential Manager (triggers Windows Hello)
/// - `1password:<account>` — load from 1Password (triggers `op` CLI auth)
/// - anything else — treat as a file path
pub fn load_signer(source: &str) -> Result<MemorySigner> {
    load_signer_with_reason(source, "Authorize a payment with pay.")
}

/// Load a signer for a payment, prefixing rejection errors with the amount
/// (e.g. "$0.10 payment authorization was rejected by user at Apple Keychain").
pub fn load_signer_for_payment(source: &str, amount: &str, desc: &str) -> Result<MemorySigner> {
    let reason = format!("Authorize payment of {amount} for {desc}.");
    load_signer_with_reason(source, &reason).map_err(|e| match e {
        Error::PaymentRejected(where_) => {
            Error::PaymentRejected(format!("{amount} payment authorization was {where_}"))
        }
        other => other,
    })
}

// ── Network-aware loaders ───────────────────────────────────────────────────

/// Resolve the wallet for a Solana network slug and return a signer.
///
/// Lookup order:
///
/// 1. **`accounts.yml` mapping** — if `networks.<network>` points at an
///    account, use that account. Keystore-backed accounts go through the
///    normal `load_signer_with_reason` path; ephemeral accounts have
///    their inline secret bytes loaded directly (no Touch ID, no prompt).
///
/// 2. **Lazy ephemeral creation** — if no mapping exists AND the network
///    is one we consider "throwaway" (`localnet` / `devnet`), generate a
///    fresh ephemeral, persist it as `accounts.<network> + networks.<network>`,
///    and return it. The returned `Option<ResolvedEphemeral>` is `Some` only
///    in this case so the caller knows to print a notice.
///
/// 3. **Mainnet without a wallet** — error. We never auto-create a wallet
///    for `mainnet`; the user must run `pay setup` to bind their real
///    wallet first. This is intentional — silently generating a mainnet
///    wallet would be a footgun.
pub fn load_signer_for_network(
    network: &str,
    store: &dyn AccountsStore,
) -> Result<(MemorySigner, Option<ResolvedEphemeral>)> {
    load_signer_for_network_with_reason(network, store, None, "Authorize a payment with pay.")
}

/// Variant of [`load_signer_for_network`] that takes an explicit reason
/// string for the keystore auth prompt (e.g.
/// "Authorize payment of $0.10 for accessing API api.example.com.").
pub fn load_signer_for_network_with_reason(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    reason: &str,
) -> Result<(MemorySigner, Option<ResolvedEphemeral>)> {
    let file = store.load()?;
    if let Some(name) = account_override {
        if let Some(account) = file.named_account_for_network(network, name).cloned() {
            let signer = load_signer_from_account_with_reason(&account, name, network, reason)?;
            return Ok((signer, None));
        }
        if is_lazy_ephemeral_network(network) {
            let resolved = load_or_create_ephemeral_for_network_as(network, name, store)?;
            let signer = signer_from_ephemeral(&resolved.account)?;
            return Ok((signer, Some(resolved)));
        }
        return Err(Error::Config(format!(
            "No account named `{name}` configured for network `{network}`."
        )));
    }
    match resolve_account_for_network(network, &file) {
        AccountChoice::Resolved { name, account } => {
            let signer = load_signer_from_account_with_reason(&account, &name, network, reason)?;
            Ok((signer, None))
        }
        AccountChoice::Missing => {
            if is_lazy_ephemeral_network(network) {
                let resolved = load_or_create_ephemeral_for_network(network, store)?;
                let signer = signer_from_ephemeral(&resolved.account)?;
                Ok((signer, Some(resolved)))
            } else {
                Err(Error::Config(format!(
                    "No account configured for network `{network}`.\n\n\
                     Run `pay setup` to create an account."
                )))
            }
        }
    }
}

/// Network-aware loader for a payment, with the same amount-prefixed
/// rejection-error rewrap as [`load_signer_for_payment`].
pub fn load_signer_for_network_payment(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    amount: &str,
    desc: &str,
) -> Result<(MemorySigner, Option<ResolvedEphemeral>)> {
    let reason = format!("Authorize payment of {amount} for {desc}.");
    load_signer_for_network_with_reason(network, store, account_override, &reason).map_err(|e| {
        match e {
            Error::PaymentRejected(where_) => {
                Error::PaymentRejected(format!("{amount} payment authorization was {where_}"))
            }
            other => other,
        }
    })
}

/// Networks where missing-entry → auto-generate-an-ephemeral is a safe
/// default. Real money networks are NOT in this list — we refuse to
/// silently create a mainnet wallet.
fn is_lazy_ephemeral_network(network: &str) -> bool {
    matches!(network, "localnet" | "devnet")
}

pub fn load_keypair_bytes_from_account_with_reason(
    account: &Account,
    name: &str,
    network: &str,
    reason: &str,
) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    if account.keystore == Keystore::Ephemeral {
        maybe_authenticate_ephemeral_account(account, network, reason)?;
        return account
            .ephemeral_keypair_bytes()
            .map(crate::keystore::Zeroizing::new)
            .ok_or_else(|| {
                Error::Config(
                    "Ephemeral account is missing its inline `secret_key_b58` field".to_string(),
                )
            });
    }

    let source = account
        .signer_source(name)
        .expect("non-ephemeral accounts must provide a signer source");

    match account.keystore {
        Keystore::AppleKeychain => {
            #[cfg(target_os = "macos")]
            {
                let ks = if account.auth_required_for_network(network) {
                    crate::keystore::Keystore::apple_keychain()
                } else {
                    crate::keystore::Keystore::new(
                        crate::keystore::auth::NoAuth,
                        crate::keystore::macos::AppleKeychainStore,
                        false,
                    )
                };
                ks.load_keypair(name, reason)
                    .map_err(|e| Error::Config(format!("keychain: {e}")))
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(Error::Config(
                    "Keychain not available on this platform".to_string(),
                ))
            }
        }
        Keystore::GnomeKeyring => {
            #[cfg(target_os = "linux")]
            {
                let ks = if account.auth_required_for_network(network) {
                    crate::keystore::Keystore::gnome_keyring()
                } else {
                    crate::keystore::Keystore::new(
                        crate::keystore::auth::NoAuth,
                        crate::keystore::linux::SecretServiceStore,
                        false,
                    )
                };
                ks.load_keypair(name, reason)
                    .map_err(|e| Error::Config(format!("gnome-keyring: {e}")))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = source;
                Err(Error::Config(
                    "GNOME Keyring not available on this platform".to_string(),
                ))
            }
        }
        Keystore::WindowsHello => {
            #[cfg(target_os = "windows")]
            {
                let ks = if account.auth_required_for_network(network) {
                    crate::keystore::Keystore::windows_hello()
                } else {
                    crate::keystore::Keystore::new(
                        crate::keystore::auth::NoAuth,
                        crate::keystore::windows::WindowsCredentialStore,
                        false,
                    )
                };
                ks.load_keypair(name, reason)
                    .map_err(|e| Error::Config(format!("windows-hello: {e}")))
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = source;
                Err(Error::Config(
                    "Windows Hello not available on this platform".to_string(),
                ))
            }
        }
        Keystore::OnePassword => {
            let op_account = account.account.clone();
            let ks = if let Some(vault) = &account.vault {
                crate::keystore::Keystore::onepassword_with_vault(vault.clone(), op_account)
            } else {
                crate::keystore::Keystore::onepassword(op_account)
            };
            ks.load_keypair(name, reason).map_err(|e| {
                if matches!(e, crate::keystore::Error::AuthDenied(_)) {
                    Error::PaymentRejected("rejected by user at 1Password".to_string())
                } else {
                    Error::Config(format!("1password: {e}"))
                }
            })
        }
        Keystore::File => load_signer_keypair_bytes_with_reason(&source, reason),
        Keystore::Ephemeral => unreachable!("handled above"),
    }
}

pub fn load_signer_from_account_with_reason(
    account: &Account,
    name: &str,
    network: &str,
    reason: &str,
) -> Result<MemorySigner> {
    let bytes = load_keypair_bytes_from_account_with_reason(account, name, network, reason)?;
    MemorySigner::from_bytes(&bytes).map_err(|e| Error::Config(format!("Invalid keypair: {e}")))
}

fn signer_from_ephemeral(account: &Account) -> Result<MemorySigner> {
    let bytes = account.ephemeral_keypair_bytes().ok_or_else(|| {
        Error::Config("Ephemeral account is missing its inline `secret_key_b58` field".to_string())
    })?;
    MemorySigner::from_bytes(&bytes)
        .map_err(|e| Error::Config(format!("Invalid ephemeral keypair bytes: {e}")))
}

fn maybe_authenticate_ephemeral_account(
    account: &Account,
    network: &str,
    reason: &str,
) -> Result<()> {
    if !account.auth_required_for_network(network) {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        crate::keystore::Keystore::apple_keychain()
            .authenticate(reason)
            .map_err(map_ephemeral_auth_error)
    }

    #[cfg(target_os = "linux")]
    {
        crate::keystore::Keystore::gnome_keyring()
            .authenticate(reason)
            .map_err(map_ephemeral_auth_error)
    }

    #[cfg(target_os = "windows")]
    {
        crate::keystore::Keystore::windows_hello()
            .authenticate(reason)
            .map_err(map_ephemeral_auth_error)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = reason;
        Err(Error::Config(
            "Ephemeral account auth gating is not available on this platform".to_string(),
        ))
    }
}

fn map_ephemeral_auth_error(e: crate::keystore::Error) -> Error {
    if matches!(e, crate::keystore::Error::AuthDenied(_)) {
        Error::PaymentRejected("rejected by user at authentication prompt".to_string())
    } else {
        Error::Config(format!("ephemeral auth gate: {e}"))
    }
}

/// Load a `MemorySigner` with a custom reason string.
pub fn load_signer_with_reason(source: &str, reason: &str) -> Result<MemorySigner> {
    let bytes = load_signer_keypair_bytes_with_reason(source, reason)?;
    MemorySigner::from_bytes(&bytes).map_err(|e| Error::Config(format!("Invalid keypair: {e}")))
}

pub fn load_signer_keypair_bytes_with_reason(
    source: &str,
    reason: &str,
) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
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

/// Human-readable name of the auth UI for a given keystore backend, used in
/// "Payment rejected" messages when the user cancels at the OS prompt.
fn rejection_source(backend: &str) -> &'static str {
    match backend {
        "keychain" => "rejected by user at Apple Keychain",
        "windows-hello" => "rejected by user at Windows Hello",
        "gnome-keyring" => "rejected by user at GNOME Keyring",
        "1password" => "rejected by user at 1Password",
        _ => "rejected by user at authentication prompt",
    }
}

fn load_from_file(path: &str) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    let expanded = shellexpand::tilde(path);
    // Newer solana-keychain split file vs inline-string parsing into two
    // separate constructors. Prefer the file path when the argument exists
    // on disk; otherwise fall back to treating the source as an inline
    // private key (base58 or u8-array literal).
    if std::path::Path::new(expanded.as_ref()).exists() {
        let data = std::fs::read_to_string(expanded.as_ref())
            .map_err(|e| Error::Config(format!("Failed to load keypair from {path}: {e}")))?;
        parse_private_key_string(&data)
            .map(crate::keystore::Zeroizing::new)
            .map_err(|e| Error::Config(format!("Failed to load keypair from {path}: {e}")))
    } else {
        parse_private_key_string(expanded.as_ref())
            .map(crate::keystore::Zeroizing::new)
            .map_err(|e| Error::Config(format!("Failed to load keypair from {path}: {e}")))
    }
}

fn parse_private_key_string(input: &str) -> std::result::Result<Vec<u8>, String> {
    let trimmed = input.trim();

    if trimmed.starts_with('[') {
        let bytes: Vec<u8> =
            serde_json::from_str(trimmed).map_err(|e| format!("Invalid keypair JSON: {e}"))?;
        if bytes.len() != 64 {
            return Err(format!("Expected 64 bytes, got {}", bytes.len()));
        }
        return Ok(bytes);
    }

    let bytes = bs58::decode(trimmed)
        .into_vec()
        .map_err(|e| format!("Invalid base58 private key: {e}"))?;
    if bytes.len() != 64 {
        return Err(format!("Expected 64 bytes, got {}", bytes.len()));
    }
    Ok(bytes)
}

fn load_from_keystore_backend(
    backend: &str,
    account: &str,
    reason: &str,
) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    let keystore = match backend {
        #[cfg(target_os = "macos")]
        "keychain" => crate::keystore::Keystore::apple_keychain(),
        #[cfg(not(target_os = "macos"))]
        "keychain" => {
            return Err(Error::Config(
                "Keychain not available on this platform".to_string(),
            ));
        }

        #[cfg(target_os = "linux")]
        "gnome-keyring" => crate::keystore::Keystore::gnome_keyring(),
        #[cfg(not(target_os = "linux"))]
        "gnome-keyring" => {
            return Err(Error::Config(
                "GNOME Keyring not available on this platform".to_string(),
            ));
        }

        #[cfg(target_os = "windows")]
        "windows-hello" => crate::keystore::Keystore::windows_hello(),
        #[cfg(not(target_os = "windows"))]
        "windows-hello" => {
            return Err(Error::Config(
                "Windows Hello not available on this platform".to_string(),
            ));
        }

        "1password" => crate::keystore::Keystore::onepassword(None),

        _ => {
            return Err(Error::Config(format!(
                "Unknown keystore backend: {backend}"
            )));
        }
    };

    let bytes = keystore.load_keypair(account, reason).map_err(|e| {
        if matches!(e, crate::keystore::Error::AuthDenied(_)) {
            Error::PaymentRejected(rejection_source(backend).to_string())
        } else {
            Error::Config(format!("{backend}: {e}"))
        }
    })?;

    Ok(bytes)
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
    fn load_signer_accepts_inline_private_key_string() {
        use solana_mpp::solana_keychain::SolanaSigner;

        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let mut keypair_bytes = Vec::with_capacity(64);
        keypair_bytes.extend_from_slice(&signing_key.to_bytes());
        keypair_bytes.extend_from_slice(&verifying_key.to_bytes());
        let inline = bs58::encode(keypair_bytes).into_string();

        let signer = load_signer(&inline).unwrap();
        let expected_pubkey = bs58::encode(verifying_key.to_bytes()).into_string();
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
    }

    #[test]
    fn load_signer_windows_hello_unavailable() {
        #[cfg(not(target_os = "windows"))]
        {
            let result = load_signer("windows-hello:default");
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("not available on this platform")
            );
        }
    }

    // ── load_signer_for_network ────────────────────────────────────────────

    use crate::accounts::{Account, AccountsFile, MAINNET_NETWORK, MemoryAccountsStore};

    fn fresh_ephemeral_account() -> Account {
        // Build an ephemeral account directly so the test doesn't depend
        // on the lazy-create internals.
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let mut full = Vec::with_capacity(64);
        full.extend_from_slice(&signing_key.to_bytes());
        full.extend_from_slice(&verifying_key.to_bytes());
        Account {
            keystore: Keystore::Ephemeral,
            active: false,
            auth_required: Some(false),
            pubkey: Some(bs58::encode(verifying_key.to_bytes()).into_string()),
            vault: None,
            account: None,
            path: None,
            secret_key_b58: Some(bs58::encode(&full).into_string()),
            created_at: Some("2026-04-10T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn load_signer_for_network_resolves_existing_ephemeral() {
        let mut file = AccountsFile::default();
        let acct = fresh_ephemeral_account();
        let expected_pubkey = acct.pubkey.clone().unwrap();
        file.upsert("localnet", "default", acct);
        let store = MemoryAccountsStore::with_file(file);

        let (signer, ephemeral) = load_signer_for_network("localnet", &store).unwrap();
        use solana_mpp::solana_keychain::SolanaSigner;
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
        assert!(
            ephemeral.is_none(),
            "must NOT report a creation when the entry already existed"
        );
        assert_eq!(store.save_count(), 0, "no writes on cache hit");
    }

    #[test]
    fn load_signer_for_network_lazy_creates_localnet() {
        // No mapping → auto-create + persist + return Some(ResolvedEphemeral).
        let store = MemoryAccountsStore::new();
        let (signer, ephemeral) = load_signer_for_network("localnet", &store).unwrap();
        use solana_mpp::solana_keychain::SolanaSigner;

        let resolved = ephemeral.expect("ephemeral creation must be reported");
        assert!(resolved.created);
        assert_eq!(resolved.network, "localnet");
        assert_eq!(
            resolved.account.pubkey.as_deref(),
            Some(signer.pubkey().to_string().as_str())
        );
        assert_eq!(
            store.save_count(),
            1,
            "lazy create must persist exactly once"
        );
    }

    #[test]
    fn load_signer_for_network_lazy_creates_devnet() {
        let store = MemoryAccountsStore::new();
        let (_, ephemeral) = load_signer_for_network("devnet", &store).unwrap();
        let resolved = ephemeral.expect("devnet must lazy-create");
        assert_eq!(resolved.network, "devnet");
        assert!(resolved.created);
    }

    #[test]
    fn load_signer_for_network_refuses_to_create_mainnet() {
        // Real money: never silently create. User must run `pay setup`.
        let store = MemoryAccountsStore::new();
        let err = load_signer_for_network(MAINNET_NETWORK, &store).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No account configured"),
            "missing setup hint: {msg}"
        );
        assert!(msg.contains("pay setup"), "missing setup command: {msg}");
        assert_eq!(
            store.save_count(),
            0,
            "must not write to store on mainnet miss"
        );
    }

    #[test]
    fn load_signer_for_network_lazy_creates_when_missing() {
        // No account for localnet → auto-create an ephemeral.
        let store = MemoryAccountsStore::new();
        let (_, ephemeral) = load_signer_for_network("localnet", &store).unwrap();
        let resolved = ephemeral.expect("ephemeral creation must be reported");
        assert!(resolved.created);
        assert_eq!(resolved.network, "localnet");
        assert_eq!(store.save_count(), 1);
    }

    #[test]
    fn load_signer_for_network_caches_lazy_created_keypair() {
        // First call creates, second call must hit the cache (same pubkey,
        // no new write).
        let store = MemoryAccountsStore::new();
        let (signer1, e1) = load_signer_for_network("localnet", &store).unwrap();
        let (signer2, e2) = load_signer_for_network("localnet", &store).unwrap();

        use solana_mpp::solana_keychain::SolanaSigner;
        assert_eq!(signer1.pubkey().to_string(), signer2.pubkey().to_string());
        assert!(e1.is_some(), "first call should report creation");
        assert!(e2.is_none(), "second call must be a cache hit");
        assert_eq!(store.save_count(), 1, "exactly one write across both calls");
    }

    #[test]
    fn load_signer_for_network_resolves_named_existing_ephemeral() {
        let mut file = AccountsFile::default();
        let acct = fresh_ephemeral_account();
        let expected_pubkey = acct.pubkey.clone().unwrap();
        file.upsert("localnet", "alice", acct);
        let store = MemoryAccountsStore::with_file(file);

        let (signer, ephemeral) =
            load_signer_for_network_with_reason("localnet", &store, Some("alice"), "test").unwrap();

        use solana_mpp::solana_keychain::SolanaSigner;
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
        assert!(
            ephemeral.is_none(),
            "existing named account must not report creation"
        );
        assert_eq!(
            store.save_count(),
            0,
            "existing named account must not write"
        );
    }

    #[test]
    fn load_signer_for_network_lazy_creates_named_localnet_account() {
        let store = MemoryAccountsStore::new();
        let (signer, ephemeral) =
            load_signer_for_network_with_reason("localnet", &store, Some("alice"), "test").unwrap();

        use solana_mpp::solana_keychain::SolanaSigner;
        let resolved = ephemeral.expect("named localnet miss must create");
        assert!(resolved.created);
        assert_eq!(resolved.account_name, "alice");
        assert_eq!(resolved.network, "localnet");
        assert_eq!(
            resolved.account.pubkey.as_deref(),
            Some(signer.pubkey().to_string().as_str())
        );

        let snapshot = store.snapshot();
        assert!(
            snapshot
                .named_account_for_network("localnet", "alice")
                .is_some()
        );
        assert_eq!(store.save_count(), 1);
    }

    #[test]
    fn load_signer_for_network_rejects_missing_named_mainnet_account() {
        let store = MemoryAccountsStore::new();
        let err =
            load_signer_for_network_with_reason(MAINNET_NETWORK, &store, Some("alice"), "test")
                .unwrap_err();

        assert!(
            err.to_string()
                .contains("No account named `alice` configured for network `mainnet`.")
        );
        assert_eq!(store.save_count(), 0);
    }

    #[test]
    fn signer_from_ephemeral_rejects_missing_inline_secret() {
        let account = Account {
            keystore: Keystore::Ephemeral,
            active: false,
            auth_required: Some(false),
            pubkey: None,
            vault: None,
            path: None,
            account: None,
            secret_key_b58: None,
            created_at: None,
        };

        let err = signer_from_ephemeral(&account).unwrap_err();
        assert!(
            err.to_string()
                .contains("Ephemeral account is missing its inline `secret_key_b58` field")
        );
    }

    #[test]
    fn rejection_source_maps_known_backends() {
        assert_eq!(
            rejection_source("keychain"),
            "rejected by user at Apple Keychain"
        );
        assert_eq!(
            rejection_source("windows-hello"),
            "rejected by user at Windows Hello"
        );
        assert_eq!(
            rejection_source("gnome-keyring"),
            "rejected by user at GNOME Keyring"
        );
        assert_eq!(
            rejection_source("1password"),
            "rejected by user at 1Password"
        );
        assert_eq!(
            rejection_source("unknown"),
            "rejected by user at authentication prompt"
        );
    }
}
