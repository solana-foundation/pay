//! `pay export` — export keypair in Solana CLI format.

use pay_core::keystore::Keystore;

/// Export your keypair to a JSON file (Solana CLI format).
///
/// The output is a JSON array of 64 bytes — the same format used by
/// `solana-keygen` and expected by `--keypair` in the Solana CLI.
///
/// Examples:
///   pay export key.json
///   pay export -                # print to stdout
#[derive(clap::Args)]
pub struct ExportCommand {
    /// Output file path, or "-" for stdout.
    pub path: String,

    /// Account name to export. Defaults to the default account.
    #[arg(long)]
    pub name: Option<String>,
}

impl ExportCommand {
    pub fn run(self, keypair_source: Option<&str>) -> pay_core::Result<()> {
        let source = if let Some(name) = &self.name {
            let accounts = pay_core::accounts::AccountsFile::load()?;
            let (_, account) = accounts
                .accounts
                .iter()
                .find(|(n, _)| *n == name)
                .ok_or_else(|| pay_core::Error::Config(format!("Account '{name}' not found")))?;
            // Ephemeral accounts can't be exported through this path —
            // they have no external signer source. Tell the user to read
            // the secret_key_b58 directly from accounts.yml.
            account.signer_source(name).ok_or_else(|| {
                pay_core::Error::Config(format!(
                    "Account '{name}' is an ephemeral wallet — its secret key is \
                     stored inline in ~/.config/pay/accounts.yml under the \
                     `secret_key_b58` field"
                ))
            })?
        } else {
            let config = pay_core::Config::load().unwrap_or_default();
            keypair_source
                .map(|s| s.to_string())
                .or_else(|| config.default_keypair_source())
                .ok_or_else(|| {
                    pay_core::Error::Config(
                        "No wallet configured. Run `pay setup` first.".to_string(),
                    )
                })?
        };

        let signer = pay_core::signer::load_signer(&source)?;

        use solana_mpp::solana_keychain::SolanaSigner;
        let pubkey = signer.pubkey();

        let keypair_bytes = reload_raw_bytes(&source)?;

        let json = serde_json::to_string(&*keypair_bytes)
            .map_err(|e| pay_core::Error::Config(format!("JSON error: {e}")))?;

        if self.path == "-" {
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

                let mut file = opts.open(&self.path).map_err(|e| {
                    pay_core::Error::Config(format!("Failed to create {}: {e}", self.path))
                })?;
                writeln!(file, "{json}").map_err(|e| {
                    pay_core::Error::Config(format!("Failed to write {}: {e}", self.path))
                })?;
            }
            eprintln!("Exported to {} (pubkey: {})", self.path, pubkey);
        }

        Ok(())
    }
}

fn reload_raw_bytes(source: &str) -> pay_core::Result<pay_core::keystore::Zeroizing<Vec<u8>>> {
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
    ks.load_keypair(account, "export keypair")
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

        "1password" => Ok(Keystore::onepassword()),

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
