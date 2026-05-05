//! `pay account list` — list all accounts with balances.

use owo_colors::OwoColorize;

use crate::components::{
    explorer_link, format_account_header, print_balance_unavailable, print_balances,
    print_topup_note,
};

const MAINNET: &str = "mainnet";
const BALANCE_INDENT: &str = "    ";

/// List all registered accounts.
#[derive(clap::Args)]
pub struct ListCommand;

impl ListCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let accounts = pay_core::accounts::AccountsFile::load()?;

        if accounts.accounts.is_empty() {
            eprintln!(
                "{}",
                "No accounts found. Run `pay account new` to create one.".dimmed()
            );
            return Ok(());
        }

        print_account_list(&accounts, None::<Highlight>);
        Ok(())
    }
}

/// How to highlight a specific account row (network + name pair).
pub enum Highlight<'a> {
    /// Show the account name in green (e.g. after import/default change).
    Green { network: &'a str, name: &'a str },
    /// Show the account name in red (e.g. before deletion).
    Red { network: &'a str, name: &'a str },
}

/// Print the account list grouped by network, with an optional highlighted row.
pub fn print_account_list(
    accounts: &pay_core::accounts::AccountsFile,
    highlight: Option<Highlight>,
) {
    use std::collections::HashMap;

    let config = pay_core::Config::load().unwrap_or_default();
    let rpc_url = config
        .rpc_url
        .clone()
        .unwrap_or_else(pay_core::balance::mainnet_rpc_url);

    let rt = tokio::runtime::Runtime::new().ok();

    // Cache stablecoin balances by pubkey to avoid duplicate pay-api calls.
    let mut balance_cache: HashMap<String, Option<pay_core::client::balance::AccountBalances>> =
        HashMap::new();

    if let Some(rt) = &rt {
        // Group unique pubkeys by their network's RPC URL so pay-api receives
        // the correct network for each account.
        let mut by_rpc: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (network, named_accounts) in &accounts.accounts {
            let network_rpc = match network.as_str() {
                "mainnet" => rpc_url.clone(),
                "localnet" => pay_core::config::SANDBOX_RPC_URL.to_string(),
                "devnet" => "https://api.devnet.solana.com".to_string(),
                _ => rpc_url.clone(),
            };
            for account in named_accounts.values() {
                if let Some(pubkey) = &account.pubkey {
                    by_rpc
                        .entry(network_rpc.clone())
                        .or_default()
                        .push(pubkey.clone());
                }
            }
        }
        // Deduplicate within each group
        for pubkeys in by_rpc.values_mut() {
            pubkeys.sort_unstable();
            pubkeys.dedup();
        }

        // One stablecoin balance batch per RPC endpoint, all concurrent.
        let results_vec = rt.block_on(async {
            let mut set = tokio::task::JoinSet::new();
            for (rpc, pubkeys) in by_rpc {
                set.spawn(async move {
                    pay_core::balance::get_stablecoin_balances_batch(&rpc, &pubkeys).await
                });
            }
            let mut out = Vec::new();
            while let Some(Ok(results)) = set.join_next().await {
                out.push(results);
            }
            out
        });
        for results in results_vec {
            for (pk, bal) in results {
                balance_cache.insert(pk, Some(bal));
            }
        }
    }

    // Track whether any mainnet account had a non-zero stablecoin balance —
    // used to surface a single yellow "run `pay topup`" hint at the end.
    let mut any_mainnet_funded = false;
    let mut mainnet_seen = false;

    for (network, named_accounts) in &accounts.accounts {
        eprintln!("{}:", network);

        for (name, account) in named_accounts {
            // Determine if this is the active account for its network:
            // - explicitly marked active, or
            // - only one account in network, or
            // - first account and none is explicitly active
            let any_active = named_accounts.values().any(|a| a.active);
            let is_active = if any_active {
                account.active
            } else {
                named_accounts
                    .iter()
                    .next()
                    .map(|(n, _)| n == name)
                    .unwrap_or(false)
            };

            let is_highlighted = match &highlight {
                Some(Highlight::Green {
                    network: hn,
                    name: n,
                })
                | Some(Highlight::Red {
                    network: hn,
                    name: n,
                }) => *hn == network.as_str() && *n == name.as_str(),
                None => false,
            };
            let is_red = matches!(
                &highlight,
                Some(Highlight::Red { network: hn, name: n })
                    if *hn == network.as_str() && *n == name.as_str()
            );

            let marker = if is_active {
                "● ".green().to_string()
            } else {
                "  ".to_string()
            };

            let name_styled = if is_red {
                name.red().bold().to_string()
            } else if is_highlighted {
                name.green().bold().to_string()
            } else if is_active {
                name.bold().to_string()
            } else {
                name.to_string()
            };

            let pubkey = account.pubkey.as_deref().unwrap_or("(no pubkey)");
            eprintln!(
                "{marker}{}",
                format_account_header(&name_styled, network, pubkey)
            );

            let bal = account
                .pubkey
                .as_ref()
                .and_then(|pk| balance_cache.get(pk))
                .and_then(|b| b.as_ref());
            let funded = match bal {
                Some(b) if b.tokens_unavailable => {
                    print_balance_unavailable(BALANCE_INDENT, account.pubkey.as_deref(), &rpc_url);
                    false
                }
                Some(b) => print_balances(b, BALANCE_INDENT),
                None => {
                    print_balance_unavailable(BALANCE_INDENT, account.pubkey.as_deref(), &rpc_url);
                    false
                }
            };

            if network == MAINNET {
                mainnet_seen = true;
                if funded {
                    any_mainnet_funded = true;
                }
            }
        }
    }

    if mainnet_seen && !any_mainnet_funded {
        print_topup_note();
    }

    eprintln!();
}

/// Format a balance for display. Reusable across list, import, etc.
///
/// Returns a colored string like "7.00 USDC" or a clickable explorer link if
/// the balance couldn't be fetched.
pub fn format_balance_display(
    bal: Option<&pay_core::client::balance::AccountBalances>,
    pubkey: Option<&str>,
    rpc_url: &str,
) -> String {
    match bal {
        Some(bal) => {
            let usdc = bal
                .tokens
                .iter()
                .find(|t| t.symbol == Some("USDC"))
                .map(|t| t.ui_amount);

            let mut parts = Vec::new();
            if let Some(amount) = usdc {
                parts.push(format!("{:.2} USDC", amount).green().to_string());
            }
            for token in &bal.tokens {
                if token.symbol == Some("USDC") {
                    continue;
                }
                let label = token.symbol.unwrap_or(&token.mint[..8]);
                parts.push(format!("{:.2} {label}", token.ui_amount));
            }
            if parts.is_empty() {
                explorer_link(pubkey, rpc_url)
            } else {
                parts.join("  ")
            }
        }
        None => explorer_link(pubkey, rpc_url),
    }
}

/// Fetch stablecoin balances for a single pubkey. Returns None on failure.
pub fn fetch_balance(pubkey: &str) -> Option<pay_core::client::balance::AccountBalances> {
    let config = pay_core::Config::load().unwrap_or_default();
    let rpc_url = config
        .rpc_url
        .clone()
        .unwrap_or_else(pay_core::balance::mainnet_rpc_url);

    let rt = tokio::runtime::Runtime::new().ok()?;
    rt.block_on(pay_core::balance::get_stablecoin_balances(&rpc_url, pubkey))
        .ok()
}
