//! `pay subscriptions status <subscription-id>` — full detail for one
//! subscription, looked up by its base58 `SubscriptionDelegation` PDA.

use owo_colors::OwoColorize;

use pay_core::accounts::AccountsFile;

use crate::components;

#[derive(clap::Args)]
pub struct StatusCommand {
    /// Base58 `subscription_id` (the `SubscriptionDelegation` PDA returned
    /// in the `Payment-Receipt` header at activation time).
    pub subscription_id: String,

    /// Emit JSON instead of the formatted view. Useful for scripting.
    #[arg(long)]
    pub json: bool,
}

impl StatusCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let accounts = AccountsFile::load()?;

        let hit = accounts
            .all_subscriptions()
            .find(|(_, _, sub)| sub.subscription_id == self.subscription_id);

        let Some((network, account, sub)) = hit else {
            return Err(pay_core::Error::Config(format!(
                "subscription `{}` is not tracked locally. \
                 Run `pay subscriptions list` to see known ids.",
                self.subscription_id
            )));
        };

        if self.json {
            // Wrap the on-disk record with its locator so consumers don't
            // have to keep two collections in sync.
            let view = serde_json::json!({
                "network": network,
                "account": account,
                "subscription": sub,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&view).unwrap_or_default()
            );
            return Ok(());
        }

        println!("{}", sub.subscription_id.bold());
        println!("  network          {network}");
        println!("  account          {account}");
        println!("  status           {}", sub.status);
        println!("  plan             {}", sub.plan_id);
        println!("  mint             {}", sub.mint);
        if let Some(curr) = &sub.currency {
            println!("  currency         {curr}");
        }
        let amount_formatted = pay_types::Stablecoin::decimals_for_mint(&sub.mint)
            .and_then(|d| {
                sub.amount_per_period
                    .parse::<u64>()
                    .ok()
                    .map(|raw| pay_core::client::send::format_token_amount(raw, d))
            })
            .unwrap_or_else(|| sub.amount_per_period.clone());
        let currency_label = sub.currency.as_deref().unwrap_or("(token)");
        println!(
            "  amount/period    {amount_formatted} {currency_label}  {}",
            format!("({} base units)", sub.amount_per_period).dimmed()
        );
        println!(
            "  period           {} {}{}",
            sub.period_count,
            sub.period_unit,
            if sub.period_count == 1 { "" } else { "s" }
        );
        println!("  recipient        {}", sub.recipient);
        println!("  puller           {}", sub.puller);
        println!("  activated        {}", sub.activated_at);
        if sub.activation_signature.is_empty() {
            println!("  activation tx    {}", "—".dimmed());
        } else {
            println!("  activation tx    {}", sub.activation_signature);
            println!(
                "    receipt        {}",
                components::solana_transaction_link(&sub.activation_signature, network)
            );
        }
        if let Some(last) = sub.last_charged_period {
            println!("  last charged     period {last}");
        }
        if let Some(exp) = &sub.expires_at {
            println!("  expires          {exp}");
        }
        if let Some(url) = &sub.resource_url {
            println!("  resource         {url}");
        }
        if let Some(desc) = &sub.description {
            println!("  description      {desc}");
        }
        Ok(())
    }
}
