//! `pay policy status [name]` — show rules + remaining daily budget.

use chrono::{Duration, Utc};
use owo_colors::OwoColorize;
use pay_core::policy::PolicyStore;

use super::{format_usd, load_policy_or_error, resolve_target_name};

#[derive(clap::Args)]
pub struct StatusCommand {
    /// Policy name. Defaults to the configured default.
    pub name: Option<String>,

    /// JSON output.
    #[arg(long)]
    pub json: bool,
}

impl StatusCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let store = pay_core::policy::FilePolicyStore::default_path();
        let file = PolicyStore::load_policies(&store)?;
        let target = resolve_target_name(self.name.as_deref(), &file)?;
        let (_file, policy) = load_policy_or_error(&store, &target)?;
        let state = PolicyStore::load_state(&store)?;
        let now = Utc::now();

        let per = state.per_policy.get(&policy.name);
        let spent_today = per.map(|s| s.spent_today).unwrap_or(0);
        let needs_reset = per
            .and_then(|s| s.day_reset_ts)
            .is_none_or(|ts| now - ts >= Duration::seconds(86_400));
        let effective_spent = if needs_reset { 0 } else { spent_today };
        let remaining = policy.daily_cap.saturating_sub(effective_spent);
        let pct_used = effective_spent
            .saturating_mul(100)
            .checked_div(policy.daily_cap)
            .map(|p| p.min(100))
            .unwrap_or(0);

        let day_resets_in = per
            .and_then(|s| s.day_reset_ts)
            .map(|ts| {
                let next = ts + Duration::seconds(86_400);
                if next > now {
                    Some(next - now)
                } else {
                    None
                }
            })
            .unwrap_or(None);

        let status_label = if policy.paused {
            "paused"
        } else if let Some(exp) = policy.expires_at
            && exp <= now
        {
            "expired"
        } else {
            "active"
        };

        if self.json || crate::no_dna::should_json(None) {
            crate::output::print_json(&serde_json::json!({
                "name": policy.name,
                "status": status_label,
                "max_per_tx_micro": policy.max_per_tx,
                "daily_cap_micro": policy.daily_cap,
                "spent_today_micro": effective_spent,
                "remaining_today_micro": remaining,
                "expires_at": policy.expires_at,
                "paused": policy.paused,
                "allowed_recipients": policy.allowed_recipients,
                "allowed_origins": policy.allowed_origins,
                "day_reset_ts": per.and_then(|s| s.day_reset_ts),
                "last_paid_at": per.and_then(|s| s.last_paid_at),
            }))?;
            return Ok(());
        }

        println!("Policy: {}", policy.name.bold());
        println!("  Status:        {status_label}");
        println!("  Per-tx cap:    {}", format_usd(policy.max_per_tx));
        println!("  Daily cap:     {}", format_usd(policy.daily_cap));
        println!(
            "  Spent today:   {} ({}%)",
            format_usd(effective_spent),
            pct_used
        );
        println!("  Remaining:     {}", format_usd(remaining));
        match day_resets_in {
            Some(d) => {
                let hours = d.num_hours();
                let mins = (d.num_minutes() - hours * 60).abs();
                println!("  Day resets:    in {hours}h {mins}m");
            }
            None => println!("  Day resets:    next paid request"),
        }
        if policy.allowed_recipients.is_empty() {
            println!("  Recipients:    {}", "(any)".dimmed());
        } else {
            println!("  Recipients:    {}", policy.allowed_recipients.join(", "));
        }
        if policy.allowed_origins.is_empty() {
            println!("  Origins:       {}", "(any)".dimmed());
        } else {
            println!("  Origins:       {}", policy.allowed_origins.join(", "));
        }
        match policy.expires_at {
            Some(exp) => {
                let remaining = exp - now;
                if remaining > Duration::zero() {
                    println!(
                        "  Expires:       {} (in {}d)",
                        exp.format("%Y-%m-%d"),
                        remaining.num_days()
                    );
                } else {
                    println!("  Expires:       {} (expired)", exp.format("%Y-%m-%d"));
                }
            }
            None => println!("  Expires:       {}", "(never)".dimmed()),
        }
        println!();
        eprintln!(
            "{}",
            "Note: spent_today is best-effort tracking by the pay CLI. \
             For actual on-chain balance, run `pay whoami`."
                .dimmed()
        );
        Ok(())
    }
}
