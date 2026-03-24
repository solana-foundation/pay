//! `pay destroy` — remove an account and its keys.

use dialoguer::Confirm;
use owo_colors::OwoColorize;
use pay_core::accounts::{Account, AccountsFile, Keystore};
use pay_core::keystore::KeystoreBackend;

/// Permanently delete an account and its secret key.
///
/// Suggests exporting the keypair first. Removes the key from the
/// keystore backend and the entry from accounts.yml.
#[derive(clap::Args)]
pub struct DestroyCommand {
    /// Account name to destroy. Defaults to your default account.
    #[arg(default_value = "default")]
    pub account: String,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

impl DestroyCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut accounts = AccountsFile::load()?;

        // If the account isn't in accounts.yml, probe keystores for legacy accounts
        if !accounts.accounts.contains_key(&self.account)
            && let Some(discovered) = discover_legacy_account(&self.account)
        {
            accounts.upsert(&self.account, discovered);
        }

        let entry = accounts.accounts.get(&self.account).ok_or_else(|| {
            let available: Vec<_> = accounts.accounts.keys().map(|k| k.as_str()).collect();
            if available.is_empty() {
                pay_core::Error::Config(
                    "No accounts found (checked accounts.yml, Keychain, and 1Password)."
                        .to_string(),
                )
            } else {
                pay_core::Error::Config(format!(
                    "Account '{}' not found. Available: {}",
                    self.account,
                    available.join(", ")
                ))
            }
        })?;

        let pubkey = entry
            .pubkey
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let keystore = entry.keystore.clone();

        eprintln!();
        eprintln!(
            "{}",
            format!(
                "  Account:  {} ({})",
                self.account,
                pubkey.chars().take(8).collect::<String>()
                    + "..."
                    + &pubkey
                        .chars()
                        .rev()
                        .take(4)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect::<String>()
            )
            .dimmed()
        );
        eprintln!("{}", format!("  Keystore: {keystore}").dimmed());
        eprintln!();

        // Suggest export
        eprintln!(
            "{}",
            "  Before destroying, consider exporting your keypair:".dimmed()
        );
        eprintln!(
            "{}",
            format!("    pay export backup-{}.json", self.account).dimmed()
        );
        eprintln!();

        if !self.yes {
            let confirmed = Confirm::new()
                .with_prompt(format!(
                    "Permanently delete account '{}'? This cannot be undone",
                    self.account
                ))
                .default(false)
                .interact()
                .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;

            if !confirmed {
                eprintln!("{}", "  Cancelled.".dimmed());
                return Ok(());
            }
        }

        // Delete from keystore backend
        match keystore {
            #[cfg(target_os = "macos")]
            Keystore::AppleKeychain => {
                use pay_core::keystore::AppleKeychain;
                AppleKeychain
                    .delete(&self.account)
                    .map_err(|e| pay_core::Error::Config(format!("Keychain delete: {e}")))?;
            }
            #[cfg(not(target_os = "macos"))]
            Keystore::AppleKeychain => {
                return Err(pay_core::Error::Config(
                    "Cannot delete Keychain entries on this platform".to_string(),
                ));
            }
            Keystore::OnePassword => {
                use pay_core::keystore::OnePassword;
                let backend = OnePassword::new();
                backend
                    .delete(&self.account)
                    .map_err(|e| pay_core::Error::Config(format!("1Password delete: {e}")))?;
            }
            Keystore::File => {
                // Don't delete user-managed files — just remove from accounts.yml
                eprintln!(
                    "{}",
                    "  File-based keypair left on disk (remove it manually if needed).".dimmed()
                );
            }
        }

        // Remove from accounts.yml
        accounts.remove(&self.account);
        accounts.save()?;

        eprintln!();
        eprintln!(
            "{}",
            format!("  Account '{}' destroyed.", self.account).dimmed()
        );

        if let Some(new_default) = &accounts.default_account {
            eprintln!(
                "{}",
                format!("  New default account: {new_default}").dimmed()
            );
        } else {
            eprintln!(
                "{}",
                "  No accounts remaining. Run `pay setup` to create a new one.".dimmed()
            );
        }
        eprintln!();

        Ok(())
    }
}

/// Probe keystores for a legacy account that predates accounts.yml.
fn discover_legacy_account(name: &str) -> Option<Account> {
    // Try macOS Keychain
    #[cfg(target_os = "macos")]
    {
        use pay_core::keystore::AppleKeychain;
        let kc = AppleKeychain;
        if kc.exists(name) {
            let pubkey = kc.pubkey(name).ok().map(|b| bs58::encode(&b).into_string());
            return Some(Account {
                keystore: Keystore::AppleKeychain,
                pubkey,
                vault: None,
                path: None,
            });
        }
    }

    // Try 1Password
    {
        use pay_core::keystore::OnePassword;
        let op = OnePassword::new();
        if op.exists(name) {
            let pubkey = op.pubkey(name).ok().map(|b| bs58::encode(&b).into_string());
            return Some(Account {
                keystore: Keystore::OnePassword,
                pubkey,
                vault: None,
                path: None,
            });
        }
    }

    None
}
