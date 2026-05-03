use crate::{components, network::SolanaNetwork};

/// Import funds from Venmo, PayPal, or a mobile wallet.
#[derive(clap::Args)]
pub struct TopupCommand {
    /// Account address to receive funds. Defaults to your mainnet account.
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

        let (pubkey, account_name) = if let Some(addr) = &self.account {
            (addr.clone(), addr.clone())
        } else {
            let accounts = pay_core::accounts::AccountsFile::load()?;
            match accounts.account_for_network(network) {
                Some((name, account)) => (
                    account.pubkey.clone().ok_or_else(|| {
                        pay_core::Error::Config("Account has no pubkey".to_string())
                    })?,
                    name.to_string(),
                ),
                None => {
                    return Err(pay_core::Error::Config(format!(
                        "No {network} account found. Run `pay setup` first."
                    )));
                }
            }
        };

        match crate::tui::run_topup_flow(&pubkey, &rpc_url, &account_name)? {
            Some(completion) => print_topup_success(&completion, network, &rpc_url),
            None => print_topup_aborted(&account_name),
        }
        Ok(())
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
    rpc_url: &str,
) -> String {
    let mut lines = Vec::new();
    if let Some(amount) = topup_received_amount(&completion.received) {
        lines.push(format!("Received {amount}"));
    }
    if let Some(hash) = &completion.tx_hash {
        let cluster = SolanaNetwork::from_slug(network).explorer_cluster(rpc_url);
        lines.push(format!(
            "{} {hash}",
            components::solana_transaction_link(hash, &cluster)
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
