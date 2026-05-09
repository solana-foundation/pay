//! `pay policy create <name>` — create a new spending policy.

use chrono::{DateTime, Utc};
use owo_colors::OwoColorize;
use pay_core::policy::{PoliciesFile, Policy, PolicyStore};

use super::{AllowKind, classify_allow_value, parse_usd_to_micro};

#[derive(clap::Args)]
pub struct CreateCommand {
    /// Name of the policy (referenced via `--policy <name>`).
    pub name: String,

    /// Per-transaction cap in USD (e.g. `0.10` or `$0.10`).
    #[arg(long, value_name = "USD")]
    pub max_per_tx: String,

    /// Daily total cap in USD (e.g. `1.00`).
    #[arg(long, value_name = "USD")]
    pub daily_cap: String,

    /// Allowed recipient pubkey OR request origin host. Repeatable. Auto-
    /// detected: 32-byte base58 strings → recipient pubkey; everything else
    /// → request origin host.
    #[arg(long = "allow", value_name = "PUBKEY_OR_HOST", action = clap::ArgAction::Append)]
    pub allow: Vec<String>,

    /// Optional ISO-8601 expiry (e.g. `2026-12-31T23:59:59Z`). After this
    /// timestamp every paid request rejects.
    #[arg(long, value_name = "ISO8601")]
    pub expires: Option<String>,

    /// Replace an existing policy with the same name.
    #[arg(long)]
    pub force: bool,

    /// Set this policy as the default for future invocations.
    #[arg(long = "default")]
    pub set_default: bool,
}

impl CreateCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let max_per_tx = parse_usd_to_micro(&self.max_per_tx)?;
        let daily_cap = parse_usd_to_micro(&self.daily_cap)?;
        if max_per_tx == 0 {
            return Err(pay_core::Error::Config(
                "--max-per-tx must be greater than 0".to_string(),
            ));
        }
        if daily_cap < max_per_tx {
            return Err(pay_core::Error::Config(format!(
                "--daily-cap ({}) must be >= --max-per-tx ({})",
                self.daily_cap, self.max_per_tx
            )));
        }

        let expires_at = match self.expires {
            Some(ref s) => Some(parse_iso8601(s)?),
            None => None,
        };

        let mut allowed_recipients = Vec::new();
        let mut allowed_origins = Vec::new();
        for raw in &self.allow {
            match classify_allow_value(raw) {
                AllowKind::Recipient(p) => allowed_recipients.push(p),
                AllowKind::Origin(h) if h.is_empty() => {
                    return Err(pay_core::Error::Config(
                        "--allow cannot be empty".to_string(),
                    ));
                }
                AllowKind::Origin(h) => allowed_origins.push(h),
            }
        }

        let store = pay_core::policy::FilePolicyStore::default_path();
        let mut file: PoliciesFile = PolicyStore::load_policies(&store)?;
        if file.get(&self.name).is_some() && !self.force {
            return Err(pay_core::Error::Config(format!(
                "policy `{}` already exists — use --force to overwrite",
                self.name
            )));
        }

        let policy = Policy {
            name: self.name.clone(),
            max_per_tx,
            daily_cap,
            allowed_recipients,
            allowed_origins,
            expires_at,
            paused: false,
            created_at: Utc::now(),
        };
        file.upsert(policy);
        if self.set_default {
            file.set_default(&self.name).ok_or_else(|| {
                pay_core::Error::Config("default-policy mismatch after upsert".to_string())
            })?;
        }
        PolicyStore::save_policies(&store, &file)?;

        crate::components::print_notice(
            crate::components::NoticeLevel::Success,
            &format!("Created policy `{}`", self.name),
            &format!(
                "Per-tx cap: {}\nDaily cap:  {}\n\nApply it with:\n  pay --policy {} curl …\n  pay policy use {}      # set as default\n  pay policy status {}   # see remaining budget",
                super::format_usd(max_per_tx),
                super::format_usd(daily_cap),
                self.name,
                self.name,
                self.name,
            ),
        );
        eprintln!();
        eprintln!(
            "{}",
            "Note: spending policy is enforced by the pay CLI before signing. \
             Touch ID still gates each spend if `auth_required: true` on the account."
                .dimmed()
        );
        Ok(())
    }
}

fn parse_iso8601(s: &str) -> pay_core::Result<DateTime<Utc>> {
    // Accept either RFC 3339 with timezone or a bare date (interpreted as UTC midnight).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let naive = date
            .and_hms_opt(23, 59, 59)
            .ok_or_else(|| pay_core::Error::Config(format!("invalid date `{s}`")))?;
        return Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    Err(pay_core::Error::Config(format!(
        "--expires `{s}` is not a valid ISO 8601 timestamp or YYYY-MM-DD date"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_accepts_rfc3339() {
        let parsed = parse_iso8601("2026-12-31T23:59:59Z").unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-12-31T23:59:59+00:00");
    }

    #[test]
    fn iso8601_accepts_bare_date_as_end_of_day() {
        let parsed = parse_iso8601("2026-12-31").unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-12-31T23:59:59+00:00");
    }

    #[test]
    fn iso8601_rejects_garbage() {
        assert!(parse_iso8601("not-a-date").is_err());
    }
}
