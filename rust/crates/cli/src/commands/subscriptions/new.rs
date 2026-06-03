//! `pay subscriptions new` — activate a new subscription against an
//! explicit on-chain Plan PDA.
//!
//! v0 stub: flag-driven activation reuses pay-kit's
//! `build_subscription_activation_transaction` plus a direct broadcast.
//! The full implementation lands in a follow-up slice; this stub validates
//! the CLI surface so test harnesses can wire it up, and prints a clear
//! message about what's still missing.

use owo_colors::OwoColorize;

#[derive(clap::Args)]
pub struct NewCommand {
    /// Base58 of the on-chain `Plan` PDA (the spec's required `externalId`).
    #[arg(long)]
    pub plan: String,

    /// Base58 SPL Token / Token-2022 mint.
    #[arg(long)]
    pub mint: String,

    /// Server puller pubkey (must be `plan.owner` or in `plan.pullers`).
    #[arg(long)]
    pub puller: String,

    /// Recipient wallet bound to the activation transaction. Must be
    /// authorized by `plan.destinations`.
    #[arg(long)]
    pub recipient: String,

    /// Per-period charge amount in mint base units, decimal string per spec.
    #[arg(long)]
    pub amount: String,

    /// Billing period (e.g. `30d`, `2w`). `month` is rejected per the
    /// Solana subscription profile.
    #[arg(long)]
    pub period: String,

    /// Optional human-readable label echoed into the local record.
    #[arg(long)]
    pub description: Option<String>,

    /// Solana network slug (`mainnet`, `devnet`, `localnet`, `sandbox`).
    #[arg(long)]
    pub network: Option<String>,

    /// Specific account name within the resolved network.
    #[arg(long)]
    pub account: Option<String>,

    /// RPC URL override.
    #[arg(long)]
    pub rpc_url: Option<String>,

    /// Optional fee-payer pubkey when the server is sponsoring fees.
    #[arg(long)]
    pub fee_payer_key: Option<String>,
}

impl NewCommand {
    pub fn run(self) -> pay_core::Result<()> {
        // Parse the period up-front so users get fast feedback on a
        // misconfigured flag without needing to load a wallet first.
        let parsed = pay_types::metering::SubscriptionEndpoint {
            period: self.period.clone(),
            price_usd: None,
            amount_base_units: Some(self.amount.clone()),
            currency: self.mint.clone(),
            expires_at: None,
            plan_id: Some(self.plan.clone()),
            plan_id_numeric: None,
            plan_bump: None,
            plan_created_at: None,
            puller: Some(self.puller.clone()),
            recipient: Some(self.recipient.clone()),
            free_trial_days: None,
        }
        .parse_period()
        .map_err(pay_core::Error::Config)?;

        eprintln!(
            "{} subscription activation is not yet wired in this build.",
            "Not implemented:".yellow().bold()
        );
        eprintln!();
        eprintln!("Inputs accepted (validated):");
        eprintln!("  plan       {}", self.plan);
        eprintln!("  mint       {}", self.mint);
        eprintln!("  puller     {}", self.puller);
        eprintln!("  recipient  {}", self.recipient);
        eprintln!("  amount     {} (base units)", self.amount);
        eprintln!(
            "  period     {} {} ({} hours)",
            parsed.1,
            parsed.0.as_str(),
            match parsed.0 {
                pay_types::metering::SubscriptionPeriodUnit::Day => parsed.1 as u64 * 24,
                pay_types::metering::SubscriptionPeriodUnit::Week => parsed.1 as u64 * 168,
            }
        );
        eprintln!();
        eprintln!(
            "{}",
            "Next slice will plug pay-kit's build_subscription_activation_transaction\n\
             into this command and persist the resulting subscription into accounts.yml."
                .dimmed()
        );
        Ok(())
    }
}
