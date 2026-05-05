//! `pay whoami` — pass-through for the system `whoami` plus the resolved
//! pay account and its non-zero stablecoin balances.
//!
//! Account selection follows the same precedence as the rest of the CLI:
//! `--account <name>` wins, otherwise the active account for the resolved
//! network, which is `--sandbox`/`--local` (→ `localnet`),
//! `--mainnet` (→ `mainnet`), or — without any flag — `mainnet`.
//!
//! Stablecoin balances are fetched via `pay_core::balance::get_stablecoin_balances`,
//! which routes through the pay-api service (`PAY_API_URL`).

use std::process;

use owo_colors::OwoColorize;
use pay_core::accounts::{Account, AccountsFile, MAINNET_NETWORK};

use crate::components::{
    format_account_header, print_balance_unavailable, print_balances, print_topup_note,
};

#[derive(clap::Args)]
pub struct WhoamiCommand;

impl WhoamiCommand {
    pub fn run(
        self,
        network_override: Option<&str>,
        account_override: Option<&str>,
    ) -> pay_core::Result<()> {
        // 1. System `whoami` — pure pass-through.
        if let Ok(out) = process::Command::new("whoami").output() {
            print!("{}", String::from_utf8_lossy(&out.stdout));
        }

        let network = network_override.unwrap_or(MAINNET_NETWORK);

        // 2. Resolve the account: `--account <name>` if given, else the
        //    active account for the network.
        let accounts = match AccountsFile::load() {
            Ok(a) => a,
            Err(_) => {
                eprintln!("{}", "(no pay accounts configured)".dimmed());
                return Ok(());
            }
        };

        let resolved: Option<(&str, &Account)> = match account_override {
            Some(name) => accounts
                .accounts
                .get(network)
                .and_then(|net| net.get_key_value(name))
                .map(|(n, a)| (n.as_str(), a)),
            None => accounts.account_for_network(network),
        };

        let Some((name, account)) = resolved else {
            let msg = match (network, account_override) {
                (MAINNET_NETWORK, None) => "(no mainnet account — run `pay setup`)".to_string(),
                (_, None) => format!("(no {network} account configured)"),
                (_, Some(n)) => format!("(no account named '{n}' on {network})"),
            };
            eprintln!("{}", msg.dimmed());
            return Ok(());
        };

        let Some(pubkey) = account.pubkey.as_deref() else {
            eprintln!("{}", format!("({network}/{name} has no pubkey)").dimmed());
            return Ok(());
        };

        eprintln!();
        eprintln!("{}", format_account_header(name, network, pubkey));

        // 3. Stablecoin balances via pay-api. RPC URL must match the
        //    target network: `PAY_RPC_URL` (set by main.rs for sandbox/
        //    local) for non-mainnet, otherwise the configured mainnet RPC.
        let rpc_url = if network == MAINNET_NETWORK {
            let config = pay_core::Config::load().unwrap_or_default();
            config
                .rpc_url
                .clone()
                .unwrap_or_else(pay_core::balance::mainnet_rpc_url)
        } else {
            std::env::var("PAY_RPC_URL")
                .unwrap_or_else(|_| pay_core::config::SANDBOX_RPC_URL.to_string())
        };

        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("{}", format!("(balance lookup skipped: {e})").dimmed());
                return Ok(());
            }
        };

        match rt.block_on(pay_core::balance::get_stablecoin_balances(&rpc_url, pubkey)) {
            Ok(b) if b.tokens_unavailable => print_balance_unavailable("", Some(pubkey), &rpc_url),
            Ok(b) => {
                let any_nonzero = print_balances(&b, "");
                // Only nudge the user to top up when they're looking at
                // mainnet — `pay topup` without `--sandbox` always targets
                // mainnet, so the hint would be misleading on localnet.
                if !any_nonzero && network == MAINNET_NETWORK {
                    print_topup_note();
                }
            }
            Err(_) => print_balance_unavailable("", Some(pubkey), &rpc_url),
        }

        Ok(())
    }
}
