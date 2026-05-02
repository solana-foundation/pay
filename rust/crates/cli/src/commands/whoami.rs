//! `pay whoami` — pass-through for the system `whoami` plus the active
//! mainnet account and its non-zero stablecoin balances.
//!
//! Stablecoin balances are fetched via `pay_core::balance::get_balances`,
//! which routes through the pay-api service (`PAY_API_URL`).

use std::process;

use owo_colors::OwoColorize;
use pay_core::accounts::AccountsFile;

use crate::components::{
    format_account_header, print_balance_unavailable, print_balances, print_topup_note,
};

const MAINNET: &str = "mainnet";

#[derive(clap::Args)]
pub struct WhoamiCommand;

impl WhoamiCommand {
    pub fn run(self) -> pay_core::Result<()> {
        // 1. System `whoami` — pure pass-through.
        if let Ok(out) = process::Command::new("whoami").output() {
            print!("{}", String::from_utf8_lossy(&out.stdout));
        }

        // 2. Active mainnet account.
        let accounts = match AccountsFile::load() {
            Ok(a) => a,
            Err(_) => {
                eprintln!("{}", "(no pay accounts configured)".dimmed());
                return Ok(());
            }
        };

        let Some((name, account)) = accounts.account_for_network(MAINNET) else {
            eprintln!("{}", "(no mainnet account — run `pay setup`)".dimmed());
            return Ok(());
        };

        let Some(pubkey) = account.pubkey.as_deref() else {
            eprintln!("{}", format!("(mainnet/{name} has no pubkey)").dimmed());
            return Ok(());
        };

        eprintln!();
        eprintln!("{}", format_account_header(name, MAINNET, pubkey));

        // 3. Stablecoin balances via pay-api.
        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);

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

        match rt.block_on(pay_core::balance::get_balances(&rpc_url, pubkey)) {
            Ok(b) if b.tokens_unavailable => print_balance_unavailable("", Some(pubkey), &rpc_url),
            Ok(b) => {
                let any_nonzero = print_balances(&b, "");
                if !any_nonzero {
                    print_topup_note();
                }
            }
            Err(_) => print_balance_unavailable("", Some(pubkey), &rpc_url),
        }

        Ok(())
    }
}
