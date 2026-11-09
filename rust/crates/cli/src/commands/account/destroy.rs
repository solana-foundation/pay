//! `pay destroy` — remove an account and its keys.

use dialoguer::Confirm;
use owo_colors::OwoColorize;
use pay_core::accounts::{Account, AccountsFile, BackendKind as KeystoreKind, MAINNET_NETWORK};
use pay_core::keystore::Keystore;

/// Permanently delete an account and its secret key.
///
/// Suggests exporting the keypair first. Removes the key from the
/// keystore backend and the entry from accounts.yml.
#[derive(clap::Args)]
pub struct DestroyCommand {
    /// Account name to destroy (required).
    #[arg(value_name = "NAME")]
    pub account: String,

    /// Remove from the sandbox (localnet) network instead of mainnet.
    #[arg(long)]
    pub sandbox: bool,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

impl DestroyCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut accounts = AccountsFile::load()?;

        let network = if self.sandbox {
            "localnet"
        } else {
            MAINNET_NETWORK
        };

        // Fall back to legacy keystore probe for mainnet accounts not yet in accounts.yml.
        let in_file = accounts
            .accounts
            .get(network)
            .and_then(|net| net.get(&self.account))
            .is_some();

        if !in_file
            && network == MAINNET_NETWORK
            && let Some(discovered) = discover_legacy_account(&self.account)
        {
            accounts.upsert(MAINNET_NETWORK, &self.account, discovered);
        }

        let entry = accounts
            .accounts
            .get(network)
            .and_then(|net| net.get(&self.account))
            .ok_or_else(|| {
                let available: Vec<String> = accounts
                    .accounts
                    .get(network)
                    .map(|net| net.keys().cloned().collect())
                    .unwrap_or_default();
                if available.is_empty() {
                    pay_core::Error::Config(format!("No {network} accounts found."))
                } else {
                    pay_core::Error::Config(format!(
                        "Account '{}' not found in {network}. Available: {}",
                        self.account,
                        available.join(", ")
                    ))
                }
            })?;

        let _pubkey = entry
            .pubkey
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let keystore_kind = entry.backend.clone();
        let op_account = entry.account.clone();
        let stores_credentials = entry
            .provider
            .as_deref()
            .and_then(pay_core::remote::provider)
            .is_none_or(|p| p.requires_credentials());
        // Whether the backend can hand the keypair back at all; remote
        // custody cannot, so there is nothing to offer for export.
        let exportable = entry.descriptor().is_ok_and(|b| b.is_exportable());

        // Show account list with the target in red
        super::list::print_account_list(
            &accounts,
            Some(super::list::Highlight::Red {
                network,
                name: &self.account,
            }),
        );

        if !self.yes {
            let theme = dialoguer::theme::ColorfulTheme::default();

            // Offer to export first, when the backend can export at all.
            let export = exportable
                && Confirm::with_theme(&theme)
                    .with_prompt(format!(
                        "Export '{}' before removing?",
                        self.account.yellow()
                    ))
                    .default(true)
                    .interact()
                    .unwrap_or(false);

            if export {
                let export_path = format!("backup-{}.json", self.account);
                let export_cmd = super::export::ExportCommand {
                    name: self.account.clone(),
                    path: Some(export_path.clone()),
                };
                // Try exporting, but don't fail the whole remove if it errors
                match export_cmd.run() {
                    Ok(()) => {}
                    Err(e) => eprintln!("  {}", format!("Export failed: {e}").dimmed()),
                }
            }

            let confirmed = Confirm::with_theme(&theme)
                .with_prompt(format!(
                    "Permanently delete '{}'? This cannot be undone",
                    self.account.red()
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
        if keystore_kind == KeystoreKind::Remote && !stores_credentials {
            // Hardware wallet: nothing was stored locally beyond accounts.yml.
            eprintln!(
                "{}",
                "  Nothing to remove from the secret store; the key stays on the device.".dimmed()
            );
        } else if keystore_kind == KeystoreKind::Remote {
            // Remove the API credential blob from the platform secret
            // store. The wallet itself stays intact at the provider.
            let intent = pay_core::keystore::AuthIntent::delete_account(&self.account);
            pay_core::remote::delete_credentials(&self.account, &intent)
                .map_err(|e| pay_core::Error::Config(format!("{keystore_kind} delete: {e}")))?;
            eprintln!(
                "{}",
                "  Credentials removed. The wallet still exists at the provider.".dimmed()
            );
        } else if let Some(ks) = keystore_for_kind(&keystore_kind, op_account)? {
            let intent = pay_core::keystore::AuthIntent::delete_account(&self.account);
            ks.delete_with_intent(&self.account, &intent)
                .map_err(|e| pay_core::Error::Config(format!("{keystore_kind} delete: {e}")))?;
        } else {
            // File-based or ephemeral — don't delete user-managed files
            eprintln!(
                "{}",
                "  File-based keypair left on disk (remove it manually if needed).".dimmed()
            );
        }

        let remaining = remove_account_record(&mut accounts, network, &self.account);

        if !remaining.is_empty() {
            let has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
            if has_tty {
                let selection =
                    dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                        .with_prompt("Choose new default account (mainnet)")
                        .items(&remaining)
                        .default(0)
                        .interact()
                        .ok();

                if let Some(idx) = selection {
                    accounts.set_active(network, &remaining[idx]);
                }
            }
        }

        accounts.save()?;

        let network_empty = accounts
            .accounts
            .get(network)
            .is_none_or(|net| net.is_empty());

        if network_empty {
            eprintln!();
            eprintln!(
                "{}",
                "  No accounts remaining. Run `pay account new` to create one.".dimmed()
            );
            eprintln!();
        } else {
            super::list::print_account_list(&accounts, None::<super::list::Highlight>);
        }

        Ok(())
    }
}

/// Build the keystore holding an account's keypair, or `None` when there
/// is nothing external to delete: file keypairs are left on disk, ephemeral
/// wallets live inside `accounts.yml`, and remote credential blobs are
/// removed through `pay_core::remote::delete_credentials`.
fn keystore_for_kind(
    kind: &KeystoreKind,
    op_account: Option<String>,
) -> pay_core::Result<Option<Keystore>> {
    use pay_core::backend::{Gate, StoreParams};

    if matches!(
        kind,
        KeystoreKind::File | KeystoreKind::Ephemeral | KeystoreKind::Remote
    ) {
        return Ok(None);
    }
    let local = pay_core::backend::local_by_kind(kind).ok_or_else(|| {
        pay_core::Error::Config(format!("No keystore backend is registered for `{kind}`."))
    })?;
    if !local.is_available() {
        return Err(pay_core::Error::Config(format!(
            "Cannot delete {} entries on this platform",
            local.display_name()
        )));
    }
    let params = StoreParams {
        op_account: op_account.as_deref(),
        ..StoreParams::default()
    };
    local.keystore(&params, Gate::Platform).map(Some)
}

/// Probe keystores for a legacy account that predates accounts.yml: the
/// OS-native store first, then 1Password.
fn discover_legacy_account(name: &str) -> Option<Account> {
    use pay_core::backend::{Gate, LocalKeystoreBackend, StoreParams};

    let candidates: Vec<&'static dyn LocalKeystoreBackend> = pay_core::backend::platform()
        .into_iter()
        .chain(std::iter::once(
            &pay_core::backend::OnePassword as &'static dyn LocalKeystoreBackend,
        ))
        .collect();

    for local in candidates {
        if !local.is_available() {
            continue;
        }
        let Ok(ks) = local.keystore(&StoreParams::default(), Gate::Platform) else {
            continue;
        };
        if !ks.exists(name) {
            continue;
        }
        let pubkey = ks.pubkey(name).ok().map(|b| bs58::encode(&b).into_string());
        return Some(Account {
            provider: None,
            backend: local.kind(),
            active: false,
            auth_required: Some(true),
            pubkey,
            vault: None,
            account: None,
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: std::collections::BTreeMap::new(),
        });
    }
    None
}

/// Removes the account record for `name` from `network` and returns the
/// remaining account names in that network when the removal took the active
/// mainnet default, so the caller can prompt for a replacement.
///
/// Only a mainnet destroy can affect the mainnet default — a sandbox destroy
/// leaves it, and every other network's records, untouched.
fn remove_account_record(accounts: &mut AccountsFile, network: &str, name: &str) -> Vec<String> {
    let was_default = network == MAINNET_NETWORK
        && accounts
            .default_account()
            .is_some_and(|(default_name, _)| default_name == name);

    accounts.remove(network, name);

    if !was_default {
        return Vec::new();
    }
    accounts
        .accounts
        .get(MAINNET_NETWORK)
        .map(|net| net.keys().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_account(active: bool) -> Account {
        Account {
            provider: None,
            backend: KeystoreKind::File,
            active,
            auth_required: None,
            pubkey: None,
            vault: None,
            account: None,
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: std::collections::BTreeMap::new(),
        }
    }

    fn accounts_with_same_name_on_both_networks() -> AccountsFile {
        let mut accounts = AccountsFile::default();
        accounts.upsert(MAINNET_NETWORK, "shop", file_account(true));
        accounts.upsert("localnet", "shop", file_account(false));
        accounts
    }

    #[test]
    fn sandbox_destroy_removes_the_localnet_record_only() {
        // The same name exists on both networks: a sandbox destroy must remove
        // the localnet record and leave the mainnet one (and its key) intact.
        let mut accounts = accounts_with_same_name_on_both_networks();

        let remaining = remove_account_record(&mut accounts, "localnet", "shop");

        assert!(remaining.is_empty());
        assert!(
            accounts
                .accounts
                .get("localnet")
                .is_none_or(|net| !net.contains_key("shop")),
            "the localnet record must be removed"
        );
        assert!(
            accounts
                .accounts
                .get(MAINNET_NETWORK)
                .is_some_and(|net| net.contains_key("shop")),
            "the mainnet record must survive a sandbox destroy"
        );
    }

    #[test]
    fn sandbox_destroy_leaves_the_mainnet_default_untouched() {
        // The mainnet "shop" is the active default: destroying the sandbox
        // "shop" must not remove it nor trigger the replacement prompt.
        let mut accounts = accounts_with_same_name_on_both_networks();

        let remaining = remove_account_record(&mut accounts, "localnet", "shop");

        assert!(remaining.is_empty());
        assert!(
            accounts
                .default_account()
                .is_some_and(|(name, _)| name == "shop"),
            "the mainnet default must survive a sandbox destroy"
        );
    }

    #[test]
    fn mainnet_destroy_of_the_default_returns_replacement_candidates() {
        let mut accounts = AccountsFile::default();
        accounts.upsert(MAINNET_NETWORK, "shop", file_account(true));
        accounts.upsert(MAINNET_NETWORK, "other", file_account(false));

        let remaining = remove_account_record(&mut accounts, MAINNET_NETWORK, "shop");

        assert_eq!(remaining, vec!["other".to_string()]);
        assert!(
            accounts
                .accounts
                .get(MAINNET_NETWORK)
                .is_some_and(|net| !net.contains_key("shop")),
            "the destroyed default must be removed from mainnet"
        );
    }

    #[test]
    fn mainnet_destroy_of_a_non_default_returns_no_candidates() {
        let mut accounts = AccountsFile::default();
        accounts.upsert(MAINNET_NETWORK, "shop", file_account(true));
        accounts.upsert(MAINNET_NETWORK, "other", file_account(false));

        let remaining = remove_account_record(&mut accounts, MAINNET_NETWORK, "other");

        assert!(remaining.is_empty());
        assert!(
            accounts
                .default_account()
                .is_some_and(|(name, _)| name == "shop"),
            "the untouched default must remain the default"
        );
    }
}
