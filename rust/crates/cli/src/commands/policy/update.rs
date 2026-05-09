//! `pay policy update <name>` — modify an existing policy in place.

use chrono::{DateTime, Utc};
use pay_core::policy::PolicyStore;

use super::{AllowKind, classify_allow_value, load_policy_or_error, parse_usd_to_micro};

#[derive(clap::Args)]
pub struct UpdateCommand {
    pub name: String,

    /// New per-tx cap.
    #[arg(long, value_name = "USD")]
    pub max_per_tx: Option<String>,

    /// New daily cap.
    #[arg(long, value_name = "USD")]
    pub daily_cap: Option<String>,

    /// Replace the entire allow list with these values. Repeatable. Mutually
    /// exclusive with `--add-allow` / `--remove-allow`.
    #[arg(long = "allow", value_name = "PUBKEY_OR_HOST", action = clap::ArgAction::Append, conflicts_with_all = ["add_allow", "remove_allow"])]
    pub allow: Vec<String>,

    /// Append values to the allow list. Repeatable.
    #[arg(long = "add-allow", value_name = "PUBKEY_OR_HOST", action = clap::ArgAction::Append)]
    pub add_allow: Vec<String>,

    /// Remove values from the allow list. Repeatable.
    #[arg(long = "remove-allow", value_name = "PUBKEY_OR_HOST", action = clap::ArgAction::Append)]
    pub remove_allow: Vec<String>,

    /// New expiry. Pass an empty string to clear.
    #[arg(long, value_name = "ISO8601")]
    pub expires: Option<String>,
}

impl UpdateCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let store = pay_core::policy::FilePolicyStore::default_path();
        let mut file = PolicyStore::load_policies(&store)?;
        let (_, mut policy) = load_policy_or_error(&store, &self.name)?;

        if let Some(s) = self.max_per_tx.as_deref() {
            policy.max_per_tx = parse_usd_to_micro(s)?;
        }
        if let Some(s) = self.daily_cap.as_deref() {
            policy.daily_cap = parse_usd_to_micro(s)?;
        }
        if policy.daily_cap < policy.max_per_tx {
            return Err(pay_core::Error::Config(
                "daily cap cannot be smaller than per-tx cap".to_string(),
            ));
        }

        if !self.allow.is_empty() {
            policy.allowed_recipients.clear();
            policy.allowed_origins.clear();
            for raw in &self.allow {
                match classify_allow_value(raw) {
                    AllowKind::Recipient(p) => policy.allowed_recipients.push(p),
                    AllowKind::Origin(h) => policy.allowed_origins.push(h),
                }
            }
        }
        for raw in &self.add_allow {
            match classify_allow_value(raw) {
                AllowKind::Recipient(p) => {
                    if !policy.allowed_recipients.contains(&p) {
                        policy.allowed_recipients.push(p);
                    }
                }
                AllowKind::Origin(h) => {
                    if !policy.allowed_origins.contains(&h) {
                        policy.allowed_origins.push(h);
                    }
                }
            }
        }
        for raw in &self.remove_allow {
            match classify_allow_value(raw) {
                AllowKind::Recipient(p) => policy.allowed_recipients.retain(|x| x != &p),
                AllowKind::Origin(h) => policy.allowed_origins.retain(|x| x != &h),
            }
        }

        if let Some(s) = self.expires.as_deref() {
            if s.trim().is_empty() {
                policy.expires_at = None;
            } else {
                policy.expires_at = Some(parse_iso8601(s)?);
            }
        }

        file.upsert(policy);
        PolicyStore::save_policies(&store, &file)?;

        crate::components::print_notice(
            crate::components::NoticeLevel::Success,
            &format!("Updated policy `{}`", self.name),
            &format!("Run `pay policy status {}` to inspect.", self.name),
        );
        Ok(())
    }
}

// Local copy — `chrono::DateTime::parse_from_rfc3339` is the canonical path,
// but we accept a bare `YYYY-MM-DD` for nicer CLI ergonomics.
fn parse_iso8601(s: &str) -> pay_core::Result<DateTime<Utc>> {
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
