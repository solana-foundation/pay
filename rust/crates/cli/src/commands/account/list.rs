//! `pay account list` — list all accounts with balances.

use owo_colors::OwoColorize;

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

/// How to highlight an account row.
pub enum Highlight<'a> {
    /// Show the account name in green (e.g. after import/default change).
    Green(&'a str),
    /// Show the account name in red (e.g. before deletion).
    Red(&'a str),
}

/// Print the account list with an optional highlighted row.
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

    // Cache balances by pubkey to avoid duplicate RPC calls (rate limiting)
    let mut balance_cache: HashMap<String, Option<pay_core::client::balance::AccountBalances>> =
        HashMap::new();

    if let Some(rt) = &rt {
        for account in accounts.accounts.values() {
            if let Some(pubkey) = &account.pubkey
                && !balance_cache.contains_key(pubkey)
            {
                let bal = rt
                    .block_on(pay_core::balance::get_balances(&rpc_url, pubkey))
                    .ok();
                balance_cache.insert(pubkey.clone(), bal);
            }
        }
    }

    eprintln!();

    for (name, account) in &accounts.accounts {
        let is_default = accounts.default_account.as_deref() == Some(name.as_str());
        let is_highlighted = match &highlight {
            Some(Highlight::Green(n)) | Some(Highlight::Red(n)) => *n == name.as_str(),
            None => false,
        };
        let is_red = matches!(&highlight, Some(Highlight::Red(n)) if *n == name.as_str());

        let marker = if is_default {
            "●".green().to_string()
        } else {
            " ".to_string()
        };

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

        let bal = account
            .pubkey
            .as_ref()
            .and_then(|pk| balance_cache.get(pk))
            .and_then(|b| b.as_ref());
        let balance_str = format_balance_display(bal, account.pubkey.as_deref());

        // Pad before colorizing so ANSI codes don't break alignment
        let name_col = format!("{:<12}", name);
        let pubkey_col = format!("{:<14}", pubkey_display);
        let backend_col = format!("{:<16}", backend);

        let name_styled = if is_red {
            name_col.red().bold().to_string()
        } else if is_highlighted {
            name_col.green().bold().to_string()
        } else if is_default {
            name_col.bold().to_string()
        } else {
            name_col.dimmed().to_string()
        };

        eprintln!(
            "  {marker} {} {} {} {balance_str}",
            name_styled,
            pubkey_col.dimmed(),
            backend_col.dimmed(),
        );
    }

    eprintln!();
}

/// Format a balance for display. Reusable across list, import, etc.
///
/// Returns a colored string like "7.00 USDC  0.1234 SOL" or a clickable
/// explorer link if the balance couldn't be fetched.
pub fn format_balance_display(
    bal: Option<&pay_core::client::balance::AccountBalances>,
    pubkey: Option<&str>,
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
            let sol = bal.sol_lamports as f64 / 1_000_000_000.0;
            if sol > 0.0 {
                parts.push(format!("{sol:.4} SOL").dimmed().to_string());
            }
            if parts.is_empty() {
                explorer_link(pubkey)
            } else {
                parts.join("  ")
            }
        }
        None => explorer_link(pubkey),
    }
}

/// Clickable terminal hyperlink to Solana Explorer token page.
pub fn explorer_link(pubkey: Option<&str>) -> String {
    match pubkey {
        Some(pk) if !pk.is_empty() => {
            let url = format!("https://explorer.solana.com/address/{pk}/tokens");
            format!("\x1b]8;;{url}\x1b\\{}\x1b]8;;\x1b\\", "balance ↗".dimmed())
        }
        _ => "—".dimmed().to_string(),
    }
}

/// Fetch balances for a single pubkey (with retry). Returns None on failure.
pub fn fetch_balance(pubkey: &str) -> Option<pay_core::client::balance::AccountBalances> {
    let config = pay_core::Config::load().unwrap_or_default();
    let rpc_url = config
        .rpc_url
        .clone()
        .unwrap_or_else(pay_core::balance::mainnet_rpc_url);

    let rt = tokio::runtime::Runtime::new().ok()?;
    rt.block_on(pay_core::balance::get_balances(&rpc_url, pubkey))
        .ok()
}
