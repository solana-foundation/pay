//! `pay account export` — export an account to a JSON key file.

use pay_core::keystore::Keystore;

/// Export an account to a JSON key file.
///
/// The output is compatible with the Solana CLI (`--keypair`).
///
/// Examples:
///   pay account export              # exports default account to ./<name>.json
///   pay account export my-key.json  # exports to a specific path
///   pay account export -            # print to stdout
#[derive(clap::Args)]
pub struct ExportCommand {
    /// Output file path, or "-" for stdout. Defaults to ./<account-name>.json.
    pub path: Option<String>,

    /// Account name to export. Defaults to the default account.
    #[arg(long)]
    pub name: Option<String>,
}

impl ExportCommand {
    pub fn run(self, active_account_name: Option<&str>) -> pay_core::Result<()> {
        let (keypair_bytes, pubkey, account_name) = if let Some(name) = &self.name {
            let accounts = pay_core::accounts::AccountsFile::load()?;
            let account = accounts
                .accounts
                .get(pay_core::accounts::MAINNET_NETWORK)
                .and_then(|net| net.iter().find(|(n, _)| *n == name))
                .map(|(_, a)| a)
                .ok_or_else(|| pay_core::Error::Config(format!("Account '{name}' not found")))?;
            let intent = pay_core::keystore::AuthIntent::export_account(name);
            let bytes = pay_core::signer::load_keypair_bytes_from_account_with_intent(
                account,
                name,
                pay_core::accounts::MAINNET_NETWORK,
                &intent,
            )?;
            let pubkey = bs58::encode(&bytes[32..64]).into_string();
            (bytes, pubkey, name.clone())
        } else {
            let config = pay_core::Config::load().unwrap_or_default();
            if active_account_name.is_none()
                && let Ok(accounts) = pay_core::accounts::AccountsFile::load()
                && let Some((name, account)) = accounts.default_account()
            {
                let intent = pay_core::keystore::AuthIntent::export_account(name);
                let bytes = pay_core::signer::load_keypair_bytes_from_account_with_intent(
                    account,
                    name,
                    pay_core::accounts::MAINNET_NETWORK,
                    &intent,
                )?;
                let pubkey = bs58::encode(&bytes[32..64]).into_string();
                (bytes, pubkey, name.to_string())
            } else {
                let src = active_account_name
                    .map(|s| s.to_string())
                    .or_else(|| config.default_active_account_name())
                    .ok_or_else(|| {
                        pay_core::Error::Config(
                            "No account configured. Run `pay setup` first.".to_string(),
                        )
                    })?;
                let fallback_name = self.name.as_deref().unwrap_or("default");
                let intent = pay_core::keystore::AuthIntent::export_account(fallback_name);
                let bytes = reload_raw_bytes(&src, &intent)?;
                let pubkey = bs58::encode(&bytes[32..64]).into_string();
                let name = self.name.clone().unwrap_or_else(|| "default".to_string());
                (bytes, pubkey, name)
            }
        };

        let short_pubkey = &pubkey[..8.min(pubkey.len())];
        let path = self
            .path
            .unwrap_or_else(|| format!("pay-account-{account_name}-{short_pubkey}.json"));

        let json = serde_json::to_string(&*keypair_bytes)
            .map_err(|e| pay_core::Error::Config(format!("JSON error: {e}")))?;

        if path == "-" {
            println!("{json}");
        } else {
            {
                use std::io::Write;
                #[cfg(unix)]
                use std::os::unix::fs::OpenOptionsExt;

                let mut opts = std::fs::OpenOptions::new();
                opts.create(true).write(true).truncate(true);
                #[cfg(unix)]
                opts.mode(0o600);

                let mut file = opts.open(&path).map_err(|e| {
                    pay_core::Error::Config(format!("Failed to create {}: {e}", path))
                })?;
                writeln!(file, "{json}").map_err(|e| {
                    pay_core::Error::Config(format!("Failed to write {}: {e}", path))
                })?;
            }
            eprintln!("Exported to {} (pubkey: {})", path, &pubkey);
        }

        Ok(())
    }
}

fn reload_raw_bytes(
    source: &str,
    intent: &pay_core::keystore::AuthIntent,
) -> pay_core::Result<pay_core::keystore::Zeroizing<Vec<u8>>> {
    // Try keystore backends
    let (backend_name, account) = if let Some(account) = source.strip_prefix("keychain:") {
        ("keychain", account)
    } else if let Some(account) = source.strip_prefix("gnome-keyring:") {
        ("gnome-keyring", account)
    } else if let Some(account) = source.strip_prefix("windows-hello:") {
        ("windows-hello", account)
    } else if let Some(account) = source.strip_prefix("1password:") {
        ("1password", account)
    } else {
        // File-based: read the Solana CLI JSON format and return raw bytes
        return reload_from_file(source);
    };

    let ks = keystore_for_backend(backend_name)?;
    ks.load_keypair_with_intent(account, intent)
        .map_err(|e| pay_core::Error::Config(format!("{backend_name}: {e}")))
}

fn keystore_for_backend(backend: &str) -> pay_core::Result<Keystore> {
    match backend {
        #[cfg(target_os = "macos")]
        "keychain" => Ok(Keystore::apple_keychain()),
        #[cfg(not(target_os = "macos"))]
        "keychain" => Err(pay_core::Error::Config(
            "Keychain not available on this platform".to_string(),
        )),

        #[cfg(target_os = "linux")]
        "gnome-keyring" => Ok(Keystore::gnome_keyring()),
        #[cfg(not(target_os = "linux"))]
        "gnome-keyring" => Err(pay_core::Error::Config(
            "GNOME Keyring not available on this platform".to_string(),
        )),

        #[cfg(target_os = "windows")]
        "windows-hello" => Ok(Keystore::windows_hello()),
        #[cfg(not(target_os = "windows"))]
        "windows-hello" => Err(pay_core::Error::Config(
            "Windows Hello not available on this platform".to_string(),
        )),

        "1password" => Ok(Keystore::onepassword(None)),

        other => Err(pay_core::Error::Config(format!("Unknown backend: {other}"))),
    }
}

fn reload_from_file(source: &str) -> pay_core::Result<pay_core::keystore::Zeroizing<Vec<u8>>> {
    let expanded = shellexpand::tilde(source);
    let data = std::fs::read_to_string(expanded.as_ref())
        .map_err(|e| pay_core::Error::Config(format!("Failed to read {source}: {e}")))?;
    let bytes: Vec<u8> = serde_json::from_str(&data)
        .map_err(|e| pay_core::Error::Config(format!("Invalid keypair JSON: {e}")))?;
    if bytes.len() != 64 {
        return Err(pay_core::Error::Config(format!(
            "Expected 64 bytes, got {}",
            bytes.len()
        )));
    }
    Ok(pay_core::keystore::Zeroizing::new(bytes))
}
