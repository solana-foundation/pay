//! `pay account import` — import an existing keypair from a Solana CLI JSON file.

use owo_colors::OwoColorize;
use pay_core::keystore::Keystore;

/// Import a keypair from a Solana CLI JSON file into a keystore backend.
#[derive(clap::Args)]
pub struct ImportCommand {
    /// Path to the Solana CLI keypair JSON file (64-byte array).
    pub file: String,

    /// Account name. Defaults to "default".
    #[arg(long, default_value = "default")]
    pub name: String,

    /// Storage backend: "keychain", "gnome-keyring", "windows-hello", "1password".
    #[arg(long)]
    pub backend: Option<String>,

    /// 1Password vault name.
    #[arg(long)]
    pub vault: Option<String>,
}

impl ImportCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let expanded = shellexpand::tilde(&self.file);
        let data = std::fs::read_to_string(expanded.as_ref())
            .map_err(|e| pay_core::Error::Config(format!("Failed to read {}: {e}", self.file)))?;
        let keypair_bytes: Vec<u8> = serde_json::from_str(&data)
            .map_err(|e| pay_core::Error::Config(format!("Invalid keypair JSON: {e}")))?;

        if keypair_bytes.len() != 64 {
            return Err(pay_core::Error::Config(format!(
                "Expected 64 bytes, got {}",
                keypair_bytes.len()
            )));
        }

        let pubkey_b58 = bs58::encode(&keypair_bytes[32..64]).into_string();

        let backend_id = match &self.backend {
            Some(b) => b.clone(),
            None => super::new::pick_backend()?,
        };

        let (ks, keystore_kind, backend_msg) = build_keystore(&backend_id, self.vault.as_deref())?;

        let sync = if backend_id == "1password" {
            pay_core::keystore::SyncMode::CloudSync
        } else {
            pay_core::keystore::SyncMode::ThisDeviceOnly
        };

        ks.import(&self.name, &keypair_bytes, sync)
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

        super::new::save_account(
            &self.name,
            keystore_kind,
            &pubkey_b58,
            self.vault,
            None,
        )?;

        eprintln!();
        eprintln!("  {} {pubkey_b58}", "Imported:".dimmed());
        eprintln!("  {}", backend_msg.dimmed());
        eprintln!();

        Ok(())
    }
}

fn build_keystore(
    backend_id: &str,
    vault: Option<&str>,
) -> pay_core::Result<(Keystore, pay_core::accounts::Keystore, &'static str)> {
    match backend_id {
        #[cfg(target_os = "macos")]
        "keychain" => Ok((
            Keystore::apple_keychain(),
            pay_core::accounts::Keystore::AppleKeychain,
            "Stored in macOS Keychain.",
        )),
        #[cfg(not(target_os = "macos"))]
        "keychain" => Err(pay_core::Error::Config("Keychain is only available on macOS".into())),

        #[cfg(target_os = "linux")]
        "gnome-keyring" => Ok((
            Keystore::gnome_keyring(),
            pay_core::accounts::Keystore::GnomeKeyring,
            "Stored in GNOME Keyring.",
        )),
        #[cfg(not(target_os = "linux"))]
        "gnome-keyring" => Err(pay_core::Error::Config("GNOME Keyring is only available on Linux".into())),

        #[cfg(target_os = "windows")]
        "windows-hello" => Ok((
            Keystore::windows_hello(),
            pay_core::accounts::Keystore::WindowsHello,
            "Stored in Windows Credential Manager.",
        )),
        #[cfg(not(target_os = "windows"))]
        "windows-hello" => Err(pay_core::Error::Config("Windows Hello is only available on Windows".into())),

        "1password" => {
            let ks = match vault {
                Some(v) => Keystore::onepassword_with_vault(v),
                None => Keystore::onepassword(),
            };
            Ok((ks, pay_core::accounts::Keystore::OnePassword, "Stored in 1Password."))
        }

        other => Err(pay_core::Error::Config(format!("Unknown backend: {other}"))),
    }
}
