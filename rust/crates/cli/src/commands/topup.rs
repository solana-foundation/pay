use crate::components;

/// Import funds from Venmo, PayPal, or a mobile wallet.
#[derive(clap::Args)]
pub struct TopupCommand {
    /// Account to fund: a configured account name, or a Solana address.
    /// Defaults to your mainnet account.
    #[arg(long)]
    pub account: Option<String>,

    /// Use the sandbox (localnet) account instead of mainnet.
    #[arg(long)]
    pub sandbox: bool,
}

impl TopupCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let config = pay_core::Config::load().unwrap_or_default();

        let (network, rpc_url) = if self.sandbox {
            let url = config
                .rpc_url
                .clone()
                .unwrap_or_else(|| pay_core::config::SANDBOX_RPC_URL.to_string());
            ("localnet", url)
        } else {
            let url = config
                .rpc_url
                .clone()
                .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
            (pay_core::accounts::MAINNET_NETWORK, url)
        };

        let accounts = pay_core::accounts::AccountsFile::load()?;
        let (pubkey, account_name) =
            resolve_destination(self.account.as_deref(), &accounts, network)?;

        match crate::tui::run_topup_flow(&pubkey, &rpc_url, &account_name)? {
            Some(completion) => print_topup_success(&completion, network, &rpc_url),
            None => print_topup_aborted(&account_name),
        }
        Ok(())
    }
}

/// Where the funds go: `--account` as a configured name on `network`
/// (falling back to a same-named remote account on mainnet, like signing
/// does), else as a raw address, else the network's default account.
fn resolve_destination(
    requested: Option<&str>,
    accounts: &pay_core::accounts::AccountsFile,
    network: &str,
) -> pay_core::Result<(String, String)> {
    let pubkey_of = |name: &str, account: &pay_core::accounts::Account| {
        account.pubkey.clone().ok_or_else(|| {
            pay_core::Error::Config(format!("Account `{name}` has no pubkey recorded."))
        })
    };
    match requested {
        Some(value) => {
            if let Some(account) =
                accounts
                    .named_account_for_network(network, value)
                    .or_else(|| {
                        accounts
                            .named_account_for_network(pay_core::accounts::MAINNET_NETWORK, value)
                            .filter(|a| a.backend == pay_core::accounts::BackendKind::Remote)
                    })
            {
                return Ok((pubkey_of(value, account)?, value.to_string()));
            }
            let is_address = bs58::decode(value)
                .into_vec()
                .map(|b| b.len() == 32)
                .unwrap_or(false);
            if is_address {
                return Ok((value.to_string(), value.to_string()));
            }
            Err(pay_core::Error::Config(format!(
                "`{value}` is neither a configured account on {network} nor a Solana address. \
                 Run `pay account list` to see your accounts."
            )))
        }
        None => match accounts.account_for_network(network) {
            Some((name, account)) => Ok((pubkey_of(name, account)?, name.to_string())),
            None => Err(pay_core::Error::Config(format!(
                "No {network} account found. Run `pay setup` first."
            ))),
        },
    }
}

pub(crate) fn print_topup_success(
    completion: &crate::tui::TopupCompletion,
    network: &str,
    rpc_url: &str,
) {
    components::print_notice(
        components::NoticeLevel::Success,
        "Account funded",
        &topup_success_body(completion, network, rpc_url),
    );
}

fn print_topup_aborted(account_name: &str) {
    components::print_notice(
        components::NoticeLevel::Warning,
        "Top-up aborted",
        &topup_aborted_body(account_name),
    );
}

fn topup_aborted_body(account_name: &str) -> String {
    format!(
        "A top-up is required before making paid requests.\n$ {}",
        topup_retry_command(account_name)
    )
}

pub(crate) fn topup_retry_command(account_name: &str) -> String {
    if account_name == pay_core::accounts::DEFAULT_ACCOUNT_NAME {
        "pay topup".to_string()
    } else {
        format!("pay topup --account {account_name}")
    }
}

pub(crate) fn topup_success_body(
    completion: &crate::tui::TopupCompletion,
    network: &str,
    _rpc_url: &str,
) -> String {
    let mut lines = Vec::new();
    if let Some(amount) = topup_received_amount(&completion.received) {
        lines.push(format!("Received {amount}"));
    }
    if let Some(hash) = &completion.tx_hash {
        lines.push(format!(
            "{} {hash}",
            components::solana_transaction_link(hash, network)
        ));
    }
    if lines.is_empty() {
        lines.push("Funds received".to_string());
    }
    lines.join("\n")
}

pub(crate) fn topup_received_amount(
    received: &pay_core::client::balance::ReceivedFunds,
) -> Option<String> {
    let amount = crate::commands::account::new::format_received(received);
    (!amount.is_empty()).then_some(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topup_aborted_body_uses_default_topup_command_for_default_account() {
        assert_eq!(
            topup_aborted_body("default"),
            "A top-up is required before making paid requests.\n$ pay topup"
        );
    }

    #[test]
    fn topup_aborted_body_uses_named_account_topup_command() {
        assert_eq!(
            topup_aborted_body("test-2"),
            "A top-up is required before making paid requests.\n$ pay topup --account test-2"
        );
    }
}

#[cfg(test)]
mod destination_tests {
    use super::*;
    use pay_core::accounts::{Account, AccountsFile, BackendKind, MAINNET_NETWORK};

    const ADDRESS: &str = "CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z";

    fn remote(pubkey: &str) -> Account {
        Account {
            backend: BackendKind::Remote,
            provider: Some("ledger".to_string()),
            active: true,
            auth_required: Some(true),
            pubkey: Some(pubkey.to_string()),
            vault: None,
            account: Some("m/44'/501'/0'".to_string()),
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: Default::default(),
        }
    }

    #[test]
    fn a_configured_name_resolves_to_its_address() {
        let mut file = AccountsFile::default();
        file.upsert(MAINNET_NETWORK, "ledger-test", remote(ADDRESS));
        let (pubkey, name) =
            resolve_destination(Some("ledger-test"), &file, MAINNET_NETWORK).unwrap();
        assert_eq!(pubkey, ADDRESS);
        assert_eq!(name, "ledger-test");
        // Remote accounts serve every network, like signing.
        let (pubkey, _) = resolve_destination(Some("ledger-test"), &file, "localnet").unwrap();
        assert_eq!(pubkey, ADDRESS);
    }

    #[test]
    fn a_raw_address_is_accepted_and_anything_else_refused() {
        let file = AccountsFile::default();
        let (pubkey, name) = resolve_destination(Some(ADDRESS), &file, MAINNET_NETWORK).unwrap();
        assert_eq!(pubkey, ADDRESS);
        assert_eq!(name, ADDRESS);
        let err = resolve_destination(Some("typo"), &file, MAINNET_NETWORK).unwrap_err();
        assert!(
            err.to_string().contains("neither a configured account"),
            "{err}"
        );
    }

    #[test]
    fn no_flag_means_the_network_default() {
        let mut file = AccountsFile::default();
        file.upsert(MAINNET_NETWORK, "main", remote(ADDRESS));
        let (pubkey, name) = resolve_destination(None, &file, MAINNET_NETWORK).unwrap();
        assert_eq!((pubkey.as_str(), name.as_str()), (ADDRESS, "main"));
        let err = resolve_destination(None, &AccountsFile::default(), MAINNET_NETWORK).unwrap_err();
        assert!(err.to_string().contains("Run `pay setup` first"), "{err}");
    }
}
