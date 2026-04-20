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

    // Cache balances by pubkey to avoid duplicate RPC calls (rate limiting)
    let mut balance_cache: HashMap<String, Option<pay_core::client::balance::AccountBalances>> =
        HashMap::new();

    if let Some(rt) = &rt {
        // Group unique pubkeys by their network's RPC URL, then send one
        // JSON-RPC batch request per RPC endpoint (concurrently).
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

        // One batch request per RPC endpoint, all concurrent
        let results_vec = rt.block_on(async {
            let mut set = tokio::task::JoinSet::new();
            for (rpc, pubkeys) in by_rpc {
                set.spawn(
                    async move { pay_core::balance::get_balances_batch(&rpc, &pubkeys).await },
                );
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

    eprintln!();

    for (network, named_accounts) in &accounts.accounts {
        // Print network header
        eprintln!("  {}:", network.dimmed());

        // Use a network-appropriate RPC URL for explorer links so localnet
        // accounts link to the sandbox explorer rather than mainnet.
        let network_rpc = match network.as_str() {
            "mainnet" => rpc_url.clone(),
            "localnet" => pay_core::config::SANDBOX_RPC_URL.to_string(),
            "devnet" => "https://api.devnet.solana.com".to_string(),
            _ => rpc_url.clone(),
        };

        for (name, account) in named_accounts {
            // Determine if this is the active account for its network:
            // - explicitly marked active, or
            // - only one account in network, or
            // - first account and none is explicitly active
            let any_active = named_accounts.values().any(|a| a.active);
            let is_active = if any_active {
                account.active
            } else {
                // First alphabetically (BTreeMap) is active by default
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
            let balance_str = format_balance_display(bal, account.pubkey.as_deref(), &network_rpc);

            // Pad before colorizing so ANSI codes don't break alignment
            let name_col = format!("{:<12}", name);
            let pubkey_col = format!("{:<14}", pubkey_display);
            let backend_col = format!("{:<16}", backend);

            let name_styled = if is_red {
                name_col.red().bold().to_string()
            } else if is_highlighted {
                name_col.green().bold().to_string()
            } else if is_active {
                name_col.bold().to_string()
            } else {
                name_col.dimmed().to_string()
            };

            eprintln!(
                "    {marker} {} {} {} {balance_str}",
                name_styled,
                pubkey_col.dimmed(),
                backend_col.dimmed(),
            );
        }
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
            let sol = bal.sol_lamports as f64 / 1_000_000_000.0;
            if sol > 0.0 {
                parts.push(format!("{sol:.4} SOL").dimmed().to_string());
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

/// Clickable terminal hyperlink to Solana Explorer token page.
///
/// For non-mainnet RPC URLs (localhost, sandbox), appends the custom cluster
/// query params so the explorer connects to the right network.
pub fn explorer_link(pubkey: Option<&str>, rpc_url: &str) -> String {
    match pubkey {
        Some(pk) if !pk.is_empty() => {
            let base = format!("https://explorer.solana.com/address/{pk}/tokens");
            let url = if rpc_url.contains("mainnet") {
                base
            } else {
                let encoded = percent_encode_rpc(rpc_url);
                format!("{base}?cluster=custom&customUrl={encoded}")
            };
            format!("\x1b]8;;{url}\x1b\\{}\x1b]8;;\x1b\\", "balance ↗".dimmed())
        }
        _ => "—".dimmed().to_string(),
    }
}

fn percent_encode_rpc(url: &str) -> String {
    url.chars()
        .map(|c| match c {
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            c => c.to_string(),
        })
        .collect()
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
