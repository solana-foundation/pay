//! `pay account import` — import an existing keypair from a Solana CLI JSON file.

use dialoguer::{Confirm, Input, theme::ColorfulTheme};
use owo_colors::OwoColorize;
use pay_core::keystore::Keystore;

/// Import a keypair from a Solana CLI JSON file into a keystore backend.
#[derive(clap::Args)]
pub struct ImportCommand {
    /// Path to the Solana CLI keypair JSON file (64-byte array).
    pub file: String,

    /// Account name. Defaults to "default".
    #[arg(long)]
    pub name: Option<String>,

    /// Storage backend: "keychain", "gnome-keyring", "windows-hello", "1password".
    #[arg(long)]
    pub backend: Option<String>,

    /// 1Password vault name.
    #[arg(long)]
    pub vault: Option<String>,
}

impl ImportCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let theme = ColorfulTheme::default();

        // 1. Read and validate keypair
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

        // 2. Display balance
        eprintln!();
        eprintln!("  {} {pubkey_b58}", "Pubkey:".dimmed());
        display_balance(&pubkey_b58);
        eprintln!();

        // 3. Resolve account name — check for conflicts
        let mut accounts = pay_core::accounts::AccountsFile::load()?;
        let name = resolve_name(&theme, self.name.as_deref(), &accounts)?;

        // 4. Pick backend and import
        let backend_id = match &self.backend {
            Some(b) => b.clone(),
            None => super::new::pick_backend()?,
        };

        let (ks, keystore_kind, _) =
            super::import::build_keystore(&backend_id, self.vault.as_deref())?;

        let sync = if backend_id == "1password" {
            pay_core::keystore::SyncMode::CloudSync
        } else {
            pay_core::keystore::SyncMode::ThisDeviceOnly
        };

        ks.import(&name, &keypair_bytes, sync)
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

        // 5. Save to accounts.yml
        let is_first = accounts.accounts.is_empty();
        accounts.upsert(
            &name,
            pay_core::accounts::Account {
                keystore: keystore_kind,
                pubkey: Some(pubkey_b58),
                vault: self.vault,
                path: None,
                secret_key_b58: None,
                created_at: None,
            },
        );

        // 6. Prompt for default (= mainnet-beta mapping) if not the
        //    only account.
        let current_mainnet = accounts.default_account().map(|(n, _)| n.to_string());
        if !is_first && current_mainnet.as_deref() != Some(name.as_str()) {
            let make_default = Confirm::with_theme(&theme)
                .with_prompt(format!("Set '{}' as the default account?", name.green()))
                .default(false)
                .interact()
                .unwrap_or(false);

            if make_default {
                accounts.set_network(pay_core::accounts::MAINNET_NETWORK, &name);
            }
        }

        accounts.save()?;

        // 7. Show the account list with the new entry highlighted
        super::list::print_account_list(&accounts, Some(super::list::Highlight::Green(&name)));

        Ok(())
    }
}

fn display_balance(pubkey: &str) {
    let bal = super::list::fetch_balance(pubkey);
    let display = super::list::format_balance_display(bal.as_ref(), Some(pubkey));
    eprintln!("  {}  {}", "Balance:".dimmed(), display);
}

fn resolve_name(
    theme: &ColorfulTheme,
    explicit: Option<&str>,
    accounts: &pay_core::accounts::AccountsFile,
) -> pay_core::Result<String> {
    let has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());

    if let Some(name) = explicit {
        if accounts.accounts.contains_key(name) && has_tty {
            let overwrite = Confirm::with_theme(theme)
                .with_prompt(format!(
                    "Account '{}' already exists. Overwrite?",
                    name.yellow()
                ))
                .default(false)
                .interact()
                .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;

            if !overwrite {
                return Err(pay_core::Error::Config("Import cancelled.".to_string()));
            }
        }
        return Ok(name.to_string());
    }

    if accounts.accounts.contains_key("default") && has_tty {
        let choice = dialoguer::Select::with_theme(theme)
            .with_prompt(format!("Account '{}' already exists", "default".yellow()))
            .items(["Overwrite 'default'", "Create with a different name"])
            .default(1)
            .interact()
            .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;

        if choice == 1 {
            let name: String = Input::with_theme(theme)
                .with_prompt("Account name")
                .interact_text()
                .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;
            return Ok(name);
        }
    }

    Ok("default".to_string())
}

pub(super) fn build_keystore(
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
        "keychain" => Err(pay_core::Error::Config(
            "Keychain is only available on macOS".into(),
        )),

        #[cfg(target_os = "linux")]
        "gnome-keyring" => Ok((
            Keystore::gnome_keyring(),
            pay_core::accounts::Keystore::GnomeKeyring,
            "Stored in GNOME Keyring.",
        )),
        #[cfg(not(target_os = "linux"))]
        "gnome-keyring" => Err(pay_core::Error::Config(
            "GNOME Keyring is only available on Linux".into(),
        )),

        #[cfg(target_os = "windows")]
        "windows-hello" => Ok((
            Keystore::windows_hello(),
            pay_core::accounts::Keystore::WindowsHello,
            "Stored in Windows Credential Manager.",
        )),
        #[cfg(not(target_os = "windows"))]
        "windows-hello" => Err(pay_core::Error::Config(
            "Windows Hello is only available on Windows".into(),
        )),

        "1password" => {
            let ks = match vault {
                Some(v) => Keystore::onepassword_with_vault(v),
                None => Keystore::onepassword(),
            };
            Ok((
                ks,
                pay_core::accounts::Keystore::OnePassword,
                "Stored in 1Password.",
            ))
        }

        other => Err(pay_core::Error::Config(format!("Unknown backend: {other}"))),
    }
}
