//! `pay policy list` — show all configured policies.

use owo_colors::OwoColorize;
use pay_core::policy::PolicyStore;

use super::format_usd;

#[derive(clap::Args)]
pub struct ListCommand {
    /// JSON output for scripting.
    #[arg(long)]
    pub json: bool,
}

impl ListCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let store = pay_core::policy::FilePolicyStore::default_path();
        let file = PolicyStore::load_policies(&store)?;
        let state = PolicyStore::load_state(&store)?;

        if file.policies.is_empty() {
            if self.json || crate::no_dna::should_json(None) {
                crate::output::print_json(&serde_json::json!({
                    "policies": [],
                    "default": null,
                }))?;
            } else {
                eprintln!(
                    "{}",
                    "No policies configured. Create one with `pay policy create <name>`.".dimmed()
                );
            }
            return Ok(());
        }

        if self.json || crate::no_dna::should_json(None) {
            let policies: Vec<_> = file
                .policies
                .values()
                .map(|p| {
                    let spent = state
                        .per_policy
                        .get(&p.name)
                        .map(|s| s.spent_today)
                        .unwrap_or(0);
                    serde_json::json!({
                        "name": p.name,
                        "paused": p.paused,
                        "max_per_tx_micro": p.max_per_tx,
                        "daily_cap_micro": p.daily_cap,
                        "spent_today_micro": spent,
                        "expires_at": p.expires_at,
                        "allowed_recipients": p.allowed_recipients,
                        "allowed_origins": p.allowed_origins,
                    })
                })
                .collect();
            crate::output::print_json(&serde_json::json!({
                "policies": policies,
                "default": file.default,
            }))?;
            return Ok(());
        }

        eprintln!(
            "{:<24} {:<10} {:>10} {:>10} {:>10}",
            "NAME".bold(),
            "STATUS".bold(),
            "PER-TX".bold(),
            "DAILY".bold(),
            "SPENT".bold(),
        );
        for policy in file.policies.values() {
            let status = if policy.paused {
                "paused".to_string()
            } else if let Some(exp) = policy.expires_at
                && exp <= chrono::Utc::now()
            {
                "expired".to_string()
            } else {
                "active".to_string()
            };
            let spent = state
                .per_policy
                .get(&policy.name)
                .map(|s| s.spent_today)
                .unwrap_or(0);
            let default_marker = if file.default.as_deref() == Some(policy.name.as_str()) {
                " (default)"
            } else {
                ""
            };
            eprintln!(
                "{:<24} {:<10} {:>10} {:>10} {:>10}",
                format!("{}{}", policy.name, default_marker),
                status,
                format_usd(policy.max_per_tx),
                format_usd(policy.daily_cap),
                format_usd(spent),
            );
        }
        Ok(())
    }
}
