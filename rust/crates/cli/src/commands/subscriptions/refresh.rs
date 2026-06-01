//! `pay subscriptions refresh` — backfill missing on-chain data on local
//! subscription records by querying RPC.
//!
//! Today this is scoped to one fixup: when `activation_signature` is empty
//! (which happens when the server emitted a receipt without
//! `activationSignature`, e.g. because it was running an older pay-kit),
//! we look up the oldest signature touching the `SubscriptionDelegation`
//! PDA and persist it back. The PDA's first transaction is the on-chain
//! `Subscribe`, which is exactly what we want for the receipt link.

use owo_colors::OwoColorize;

use pay_core::accounts::AccountsFile;
use pay_core::client::subscription::{
    default_rpc_url_for_network, lookup_activation_signature,
};

#[derive(clap::Args)]
pub struct RefreshCommand {
    /// Filter to a single account name.
    #[arg(long)]
    pub account: Option<String>,

    /// Filter to a single network slug (e.g. `mainnet`, `devnet`).
    #[arg(long)]
    pub network: Option<String>,

    /// Override the RPC URL used for the lookup. Without this, pay falls
    /// back to the canonical RPC for the subscription's network
    /// (sandbox / surfnet for `localnet`).
    #[arg(long)]
    pub rpc_url: Option<String>,
}

impl RefreshCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut accounts = AccountsFile::load()?;

        // Collect candidates up front; `all_subscriptions` borrows
        // immutably and we need to write back via `upsert_subscription`.
        let candidates: Vec<(String, String, String, String)> = accounts
            .all_subscriptions()
            .filter(|(net, name, sub)| {
                self.network.as_deref().map(|n| n == *net).unwrap_or(true)
                    && self.account.as_deref().map(|n| n == *name).unwrap_or(true)
                    && sub.activation_signature.is_empty()
            })
            .map(|(net, name, sub)| {
                (
                    net.to_string(),
                    name.to_string(),
                    sub.subscription_id.clone(),
                    sub.network.clone(),
                )
            })
            .collect();

        if candidates.is_empty() {
            eprintln!(
                "{}",
                "Nothing to refresh — every tracked subscription already has an \
                 activation_signature."
                    .dimmed()
            );
            return Ok(());
        }

        let mut updated = 0usize;
        let mut failed = 0usize;
        for (net, account_name, sub_id, sub_network) in candidates {
            match lookup_activation_signature(&sub_network, &sub_id, self.rpc_url.as_deref()) {
                Some(sig) => {
                    // Reload-and-update-in-place is overkill — the entry
                    // is still in the in-memory `accounts` map. Pull it,
                    // bump the field, write it back via upsert.
                    if let Some(mut sub) = accounts
                        .accounts
                        .get(&net)
                        .and_then(|a| a.get(&account_name))
                        .and_then(|a| a.subscriptions.get(&sub_id).cloned())
                    {
                        sub.activation_signature = sig.clone();
                        accounts.upsert_subscription(&net, &account_name, sub)?;
                        eprintln!(
                            "  {} {net}/{account_name} {} → {}",
                            "✓".green(),
                            truncate_id(&sub_id),
                            truncate_id(&sig)
                        );
                        updated += 1;
                    }
                }
                None => {
                    let rpc_url = self
                        .rpc_url
                        .clone()
                        .unwrap_or_else(|| default_rpc_url_for_network(&sub_network));
                    eprintln!(
                        "  {} {net}/{account_name} {} — no signatures at {rpc_url}",
                        "✗".red(),
                        truncate_id(&sub_id)
                    );
                    failed += 1;
                }
            }
        }

        if updated > 0 {
            accounts.save()?;
        }

        eprintln!();
        eprintln!(
            "{} {updated} refreshed, {failed} failed.",
            "Done.".bold()
        );
        Ok(())
    }
}

fn truncate_id(id: &str) -> String {
    if id.len() <= 14 {
        id.to_string()
    } else {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    }
}
