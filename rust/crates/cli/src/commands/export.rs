//! `pay export` — export keypair in Solana CLI format.

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
}

impl ExportCommand {
    pub fn run(self, keypair_source: Option<&str>) -> pay_core::Result<()> {
        let config = pay_core::Config::load().unwrap_or_default();
        let source = keypair_source
            .map(|s| s.to_string())
            .or_else(|| config.default_keypair_source())
            .ok_or_else(|| {
                pay_core::Error::Config("No wallet configured. Run `pay setup` first.".to_string())
            })?;

        let signer = pay_core::signer::load_signer(&source)?;

        use solana_mpp::solana_keychain::SolanaSigner;
        let pubkey = signer.pubkey();

        // MemorySigner stores the full 64-byte keypair — reload raw bytes
        let keypair_bytes = reload_raw_bytes(&source)?;

        // Solana CLI format: JSON array of 64 u8 values
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
    use pay_core::keystore::KeystoreBackend;

    if let Some(account) = source.strip_prefix("keychain:") {
        #[cfg(target_os = "macos")]
        {
            use pay_core::keystore::AppleKeychain;
            return AppleKeychain
                .load_keypair(account, "export keypair")
                .map_err(|e| pay_core::Error::Config(format!("Keychain: {e}")));
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = account;
            return Err(pay_core::Error::Config(
                "Keychain not available on this platform".to_string(),
            ));
        }
    }

    if let Some(account) = source.strip_prefix("gnome-keyring:") {
        #[cfg(target_os = "linux")]
        {
            use pay_core::keystore::GnomeKeyring;
            return GnomeKeyring
                .load_keypair(account, "export keypair")
                .map_err(|e| pay_core::Error::Config(format!("GNOME Keyring: {e}")));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = account;
            return Err(pay_core::Error::Config(
                "GNOME Keyring not available on this platform".to_string(),
            ));
        }
    }

    if let Some(account) = source.strip_prefix("1password:") {
        let backend = pay_core::keystore::OnePassword::new();
        return backend
            .load_keypair(account, "export keypair")
            .map_err(|e| pay_core::Error::Config(format!("1Password: {e}")));
    }

    // File-based: read the Solana CLI JSON format and return raw bytes
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
