//! `pay subscriptions list` — print every subscription tracked locally.

use owo_colors::OwoColorize;

use pay_core::accounts::{AccountsFile, Subscription, SubscriptionStatus};

use crate::components;

#[derive(clap::Args, Default)]
pub struct ListCommand {
    /// Filter to a single account name.
    #[arg(long)]
    account: Option<String>,

    /// Filter to a single network slug (e.g. `mainnet`, `devnet`).
    #[arg(long)]
    network: Option<String>,

    /// Emit JSON instead of the formatted table. Useful for scripting.
    #[arg(long)]
    json: bool,
}

impl ListCommand {
    pub fn run(self) -> pay_core::Result<()> {
        // Sweep cancelled subscriptions whose paid period has elapsed
        // (`expires_at` in the past). The on-chain delegation's
        // `expires_at_ts` is pinned at cancel time, so once we cross
        // that timestamp the local row is just stale tombstone noise
        // — the chain agrees the subscription no longer exists.
        let store = pay_core::accounts::FileAccountsStore::default_path();
        let removed = sweep_expired_cancellations(&store)?;
        if removed > 0 {
            tracing::debug!(removed, "swept expired cancelled subscriptions");
        }

        let accounts = AccountsFile::load()?;
        let rows: Vec<(&str, &str, &Subscription)> = accounts
            .all_subscriptions()
            .filter(|(net, name, _)| {
                self.network.as_deref().map(|n| n == *net).unwrap_or(true)
                    && self.account.as_deref().map(|n| n == *name).unwrap_or(true)
            })
            .collect();

        if self.json {
            // Shape the JSON ourselves so it's stable across pay versions
            // even if the on-disk `Subscription` struct grows fields.
            let json_rows: Vec<serde_json::Value> = rows
                .iter()
                .map(|(network, account, sub)| {
                    serde_json::json!({
                        "network": network,
                        "account": account,
                        "subscription_id": sub.subscription_id,
                        "plan_id": sub.plan_id,
                        "mint": sub.mint,
                        "currency": sub.currency,
                        "amount_per_period": sub.amount_per_period,
                        "period_unit": sub.period_unit,
                        "period_count": sub.period_count,
                        "status": sub.status.to_string(),
                        "expires_at": sub.expires_at,
                        "activated_at": sub.activated_at,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json_rows).unwrap_or_default()
            );
            return Ok(());
        }

        if rows.is_empty() {
            eprintln!(
                "{}",
                "No subscriptions found. Run `pay subscriptions new --help` to start one.".dimmed()
            );
            return Ok(());
        }

        for (network, account, sub) in rows {
            print_subscription_row(network, account, sub);
        }
        Ok(())
    }
}

fn print_subscription_row(network: &str, account: &str, sub: &Subscription) {
    // Mirror `pay account ls` shape: `<name_bold> [<network>]` — the
    // header used to carry a truncated subscription PDA but the full
    // id now lives on its own row below, so the chip just locates the
    // account.
    let status_styled = match sub.status {
        SubscriptionStatus::Active => sub.status.to_string().green().to_string(),
        SubscriptionStatus::Cancelled => sub.status.to_string().yellow().to_string(),
        SubscriptionStatus::Expired => sub.status.to_string().red().to_string(),
    };
    let currency = sub.currency.as_deref().unwrap_or(&sub.mint[..8]);
    let period = format!("{}{}", sub.period_count, period_short(&sub.period_unit));
    eprintln!(
        "  {} {} [{status_styled}]",
        account.bold(),
        format!("[{network}]").dimmed(),
    );

    // Full subscription id on its own line — copy-pasteable, no
    // truncation. Plan PDA isn't listed because the subscription is
    // bound to a single plan by construction and adds visual noise.
    eprintln!(
        "    {} {}",
        "Subscription Id:".dimmed(),
        sub.subscription_id
    );

    // Description after id if present — gives the viewer a "what am I
    // paying for" hook before the pricing line. Newlines / oversize
    // descriptions are flattened so the list view doesn't wrap awkwardly.
    if let Some(desc) = sub.description.as_deref().and_then(non_empty_oneline) {
        eprintln!("    {}", desc.dimmed());
    }

    // Pricing + receipt on one line: `9.99 USDC every 30d • Link to receipt ↗`
    let mut parts: Vec<String> = vec![format!(
        "{} every {period}",
        format_amount_with_currency(&sub.amount_per_period, currency, &sub.mint),
    )];
    if !sub.activation_signature.is_empty() {
        parts.push(format!(
            "{} {}",
            "\u{2022}".dimmed(),
            components::solana_transaction_link(&sub.activation_signature, network)
        ));
    }
    eprintln!("    {}", parts.join(" "));

    // "expires X" for active subscriptions, "active until X" for
    // cancelled ones — same timestamp, different framing. After cancel
    // the on-chain delegation keeps serving requests through period
    // end (`expires_at_ts`), so "active until" is what the user
    // actually wants to know.
    if let Some(exp) = &sub.expires_at {
        let label = match sub.status {
            SubscriptionStatus::Cancelled => "active until",
            _ => "expires",
        };
        eprintln!("    {label} {}", exp.dimmed());
    }
    if let Some(url) = &sub.resource_url {
        eprintln!("    {}", url.dimmed());
    }
}

/// Drop cancelled subscriptions whose paid period has fully elapsed.
/// Best-effort: rows missing / malformed `expires_at` stay put (we'd
/// rather keep a stale tombstone than delete a row whose lifecycle we
/// can't read). Returns the number of removed rows.
fn sweep_expired_cancellations(
    store: &dyn pay_core::accounts::AccountsStore,
) -> pay_core::Result<usize> {
    let mut file = store.load()?;
    let now = chrono::Utc::now();
    let mut to_remove: Vec<(String, String, String)> = Vec::new();
    for (network, by_account) in file.accounts.iter() {
        for (account, entry) in by_account.iter() {
            for (sub_id, sub) in entry.subscriptions.iter() {
                if !matches!(sub.status, SubscriptionStatus::Cancelled) {
                    continue;
                }
                let Some(expires_at) = sub.expires_at.as_deref() else {
                    continue;
                };
                let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(expires_at) else {
                    continue;
                };
                if parsed.with_timezone(&chrono::Utc) <= now {
                    to_remove.push((network.clone(), account.clone(), sub_id.clone()));
                }
            }
        }
    }
    if to_remove.is_empty() {
        return Ok(0);
    }
    let removed = to_remove.len();
    for (network, account, sub_id) in to_remove {
        file.remove_subscription(&network, &account, &sub_id);
    }
    store.save(&file)?;
    Ok(removed)
}

#[allow(dead_code)]
fn truncate_id(id: &str) -> String {
    if id.len() <= 12 {
        id.to_string()
    } else {
        format!("{}…{}", &id[..6], &id[id.len() - 4..])
    }
}

/// Collapse whitespace + trim, dropping the value entirely when empty.
/// YAML's `description: >` folds newlines into spaces but multi-paragraph
/// descriptions still survive as multi-line strings — flatten them so a
/// list row doesn't wrap into a wall of text. Truncates to 96 chars with
/// an ellipsis to keep one row = one line.
fn non_empty_oneline(value: &str) -> Option<String> {
    let folded: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() {
        return None;
    }
    const MAX: usize = 96;
    if folded.chars().count() > MAX {
        let truncated: String = folded.chars().take(MAX - 1).collect();
        Some(format!("{truncated}…"))
    } else {
        Some(folded)
    }
}

fn period_short(unit: &str) -> &'static str {
    match unit {
        "day" => "d",
        "week" => "w",
        _ => "?",
    }
}

fn format_amount_with_currency(base_units: &str, currency: &str, mint: &str) -> String {
    // Decimals are looked up via the mint (known stablecoins only); we
    // fall back to 6 because every supported stablecoin on Solana today
    // is 6-decimal, and if a future variant lands `decimals_for_mint`
    // will start returning the right value. If parsing the raw count
    // fails for any reason, surface the unformatted string so we don't
    // lie about the amount.
    let decimals = pay_types::Stablecoin::decimals_for_mint(mint).unwrap_or(6);
    match base_units.parse::<u64>() {
        Ok(raw) => format!(
            "{} {currency}",
            pay_core::client::send::format_token_amount(raw, decimals)
        ),
        Err(_) => format!("{base_units} {currency}"),
    }
}
