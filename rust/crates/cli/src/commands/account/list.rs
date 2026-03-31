//! `pay account list` — list all accounts with balances.

use owo_colors::OwoColorize;

/// List all registered accounts.
#[derive(clap::Args)]
pub struct ListCommand;

impl ListCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let accounts = pay_core::accounts::AccountsFile::load()?;

        if accounts.accounts.is_empty() {
            eprintln!("{}", "No accounts found. Run `pay account new` to create one.".dimmed());
            return Ok(());
        }

        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);

        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| pay_core::Error::Config(format!("Failed to create runtime: {e}")))?;

        eprintln!();

        for (name, account) in &accounts.accounts {
            let is_default = accounts.default_account.as_deref() == Some(name.as_str());
            let marker = if is_default { "●" } else { " " };

            let pubkey_display = account
                .pubkey
                .as_deref()
                .map(|p| {
                    if p.len() > 12 {
                        format!("{}…{}", &p[..6], &p[p.len() - 4..])
                    } else {
                        p.to_string()
                    }
                })
                .unwrap_or_else(|| "unknown".to_string());

            let backend = account.keystore.to_string();

            // Fetch balance (best-effort)
            let balance_str = if let Some(pubkey) = &account.pubkey {
                match rt.block_on(pay_core::balance::get_balances(&rpc_url, pubkey)) {
                    Ok(bal) => {
                        let mut parts = Vec::new();
                        let sol = bal.sol_lamports as f64 / 1_000_000_000.0;
                        if sol > 0.0 {
                            parts.push(format!("{sol:.4} SOL"));
                        }
                        for token in &bal.tokens {
                            let label = token.symbol.unwrap_or(&token.mint[..8]);
                            parts.push(format!("{:.2} {label}", token.ui_amount));
                        }
                        if parts.is_empty() {
                            "0 SOL".to_string()
                        } else {
                            parts.join(", ")
                        }
                    }
                    Err(_) => "—".to_string(),
                }
            } else {
                "—".to_string()
            };

            eprintln!(
                "  {marker} {name:<12} {pubkey_display:<14} {backend:<16} {balance_str}",
            );
        }

        eprintln!();
        Ok(())
    }
}
