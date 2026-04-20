/// Fund your pay account.
#[derive(clap::Args)]
pub struct TopupCommand {
    /// Account address to fund. Defaults to your mainnet account.
    #[arg(long)]
    pub account: Option<String>,

    /// Fund the sandbox (localnet) account instead of mainnet.
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

        if let Some(received) = crate::tui::run_topup_flow(&pubkey, &rpc_url, &account_name)? {
            use owo_colors::OwoColorize;
            eprintln!();
            eprintln!("  {}", "Funded!".green().bold());
            let amount = crate::commands::account::new::format_received(&received);
            if !amount.is_empty() {
                eprintln!("  {} {}", "✔".green(), amount.green());
            }
            eprintln!();
        }
        Ok(())
    }
}
