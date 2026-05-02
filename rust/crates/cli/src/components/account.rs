//! Reusable account/balance rendering — used by `pay whoami` and
//! `pay account ls` so the two views stay in sync.

use owo_colors::OwoColorize;
use pay_core::client::balance::AccountBalances;

/// Render `<name> [<network> <pubkey>]` with the bracketed location dimmed.
///
/// `name` is taken pre-styled (the caller decides bold / colour for active
/// vs inactive vs highlighted rows) so this helper just concatenates.
pub fn format_account_header(name_rendered: &str, network: &str, pubkey: &str) -> String {
    format!(
        "{} {}",
        name_rendered,
        format!("[{network} {pubkey}]").dimmed()
    )
}

/// Print one stablecoin balance per line under `indent`. When all balances
/// are zero, prints nothing and returns `false` — callers use the return
/// value to decide whether to surface a trailing "run `pay topup`" note.
pub fn print_balances(balances: &AccountBalances, indent: &str) -> bool {
    if balances.tokens.is_empty() {
        return false;
    }
    for t in &balances.tokens {
        let symbol = t.symbol.unwrap_or("?");
        eprintln!(
            "{indent}- {:<6} {}",
            symbol,
            format!("{:.2}", t.ui_amount).green()
        );
    }
    true
}

/// Fallback rendered when balance lookup failed (RPC down, pay-api
/// unreachable, etc). Prints "api offline" in yellow followed by a clickable
/// Solana Explorer link to the account's tokens page.
pub fn print_balance_unavailable(indent: &str, pubkey: Option<&str>, rpc_url: &str) {
    eprintln!(
        "{indent}{}  {}",
        "api offline".yellow(),
        explorer_link(pubkey, rpc_url)
    );
}

/// Yellow trailing note shown when every mainnet balance the caller looked at
/// came back empty. Both `pay whoami` and `pay accounts` print this once at
/// the bottom of their output.
pub fn print_topup_note() {
    eprintln!();
    eprintln!("{}", "run `pay topup` to fund a mainnet account".yellow());
}

/// Clickable terminal hyperlink to Solana Explorer's tokens page for `pubkey`.
///
/// For non-mainnet RPC URLs (localhost, sandbox), appends the custom cluster
/// query params so the explorer connects to the right network. Returns `—`
/// (dimmed) when no pubkey is available.
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
