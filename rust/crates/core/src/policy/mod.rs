//! Local spending-policy enforcement for paid HTTP requests.
//!
//! A *policy* is a named, persistent set of rules — per-tx cap, daily cap,
//! recipient/origin allowlist, expiry, and pause switch — checked inside
//! `pay` before a 402 challenge is signed. It's the persistent, richer
//! cousin of the per-invocation `--yolo-upto` cap.
//!
//! Two stores back the module:
//!   - [`PoliciesFile`] (TOML at `~/.config/pay/policies.toml`) — user-edited
//!     policy definitions plus an optional `default` selector.
//!   - [`PolicyState`] (JSON at `~/.config/pay/policy-state.json`) — rolling
//!     `spent_today` per policy, written atomically after a successful pay.
//!
//! The check itself is a pure function ([`check_payment`]) so it can be
//! unit-tested without I/O. State mutation (the daily roll) happens during
//! the check; the post-payment increment is a separate [`record_payment`]
//! call so the caller can sequence it after the HTTP retry succeeds.

pub mod check;
pub mod config;
pub mod state;
pub mod store;

pub use check::{PolicyViolation, check_payment, record_payment};
pub use config::{PoliciesFile, Policy};
pub use state::{PerPolicyState, PolicyState};
pub use store::{FilePolicyStore, MemoryPolicyStore, PolicyStore};

/// Resolved policy + a store handle, threaded into the 402 builders to gate
/// signing and persist post-payment state.
pub struct PolicyContext<'a> {
    pub policy: Policy,
    pub store: &'a dyn PolicyStore,
}

/// Extract the URL host (lowercased, no port) from a resource URL string.
/// Returns `None` when the URL is missing or unparseable — callers treat that
/// as "no origin known" for allowlist purposes.
pub fn extract_origin(resource_url: Option<&str>) -> Option<String> {
    let url = resource_url?;
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
}

/// Convert an `(amount, currency)` pair from a 402 challenge into micro-USDC.
///
/// All Solana stablecoins in the catalog are six-decimal, so the raw `amount`
/// string is already in micro-units when `currency` is a known stablecoin
/// symbol (USDC, USDT, …) or mint. Returns `Err` for non-stablecoin
/// currencies — the policy can't price SOL or other assets safely.
pub fn parse_amount_micro_usdc(amount: &str, currency: &str) -> crate::Result<u64> {
    use pay_types::Stablecoin;
    if Stablecoin::parse_symbol(currency).is_none() && Stablecoin::from_mint(currency).is_none() {
        return Err(crate::Error::PaymentRejected(format!(
            "policy is stablecoin-denominated and cannot price `{currency}` payments"
        )));
    }
    amount.parse::<u64>().map_err(|e| {
        crate::Error::PaymentRejected(format!("invalid stablecoin amount `{amount}`: {e}"))
    })
}

/// Run the policy gate (load state → check → persist daily-roll change) and
/// surface failures as [`crate::Error::PaymentRejected`] for the existing
/// CLI / MCP rejection paths.
///
/// On success, `state` may have rolled the daily window — that case is
/// persisted. The post-payment increment happens in [`record_payment_with_store`].
pub fn gate_payment_with_store(
    ctx: &PolicyContext<'_>,
    amount_micro_usdc: u64,
    recipient: &str,
    request_origin: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> crate::Result<()> {
    let mut state = ctx.store.load_state()?;
    let entry = state.entry_mut(&ctx.policy.name);
    let snapshot_reset = entry.day_reset_ts;
    let snapshot_spent = entry.spent_today;

    check_payment(&ctx.policy, entry, amount_micro_usdc, recipient, request_origin, now)
        .map_err(|v| crate::Error::PaymentRejected(format!("policy: {}", v.user_message())))?;

    let rolled =
        entry.day_reset_ts != snapshot_reset || entry.spent_today != snapshot_spent;
    if rolled {
        ctx.store.save_state(&state)?;
    }
    Ok(())
}

/// Increment `spent_today` in the persisted state after a confirmed payment.
pub fn record_payment_with_store(
    ctx: &PolicyContext<'_>,
    amount_micro_usdc: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> crate::Result<()> {
    let mut state = ctx.store.load_state()?;
    let entry = state.entry_mut(&ctx.policy.name);
    record_payment(entry, amount_micro_usdc, now);
    ctx.store.save_state(&state)?;
    Ok(())
}

/// Resolve which policy applies for the upcoming payment.
///
/// Resolution order:
///
/// 1. `--policy <name>` flag → explicit user choice (errors if name missing).
/// 2. The account's `policy` field → per-account default.
/// 3. `policies.default` → user-set global default.
/// 4. `None` → no enforcement (today's behavior).
pub fn resolve_active_policy(
    cli_override: Option<&str>,
    account_policy: Option<&str>,
    policies: &PoliciesFile,
) -> Result<Option<Policy>, crate::Error> {
    let candidate = cli_override
        .or(account_policy)
        .or(policies.default.as_deref());
    let Some(name) = candidate else {
        return Ok(None);
    };
    policies
        .get(name)
        .cloned()
        .map(Some)
        .ok_or_else(|| crate::Error::Config(format!("policy `{name}` not found in policies.toml")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn sample(name: &str) -> Policy {
        Policy {
            name: name.to_string(),
            max_per_tx: 10_000,
            daily_cap: 100_000,
            allowed_recipients: vec![],
            allowed_origins: vec![],
            expires_at: None,
            paused: false,
            created_at: t("2026-01-01T00:00:00Z"),
        }
    }

    #[test]
    fn cli_override_wins() {
        let mut file = PoliciesFile::default();
        file.upsert(sample("a"));
        file.upsert(sample("b"));
        file.set_default("a").unwrap();
        let resolved = resolve_active_policy(Some("b"), Some("a"), &file).unwrap();
        assert_eq!(resolved.unwrap().name, "b");
    }

    #[test]
    fn account_policy_used_when_no_cli_flag() {
        let mut file = PoliciesFile::default();
        file.upsert(sample("a"));
        file.upsert(sample("b"));
        file.set_default("a").unwrap();
        let resolved = resolve_active_policy(None, Some("b"), &file).unwrap();
        assert_eq!(resolved.unwrap().name, "b");
    }

    #[test]
    fn default_used_when_no_cli_or_account() {
        let mut file = PoliciesFile::default();
        file.upsert(sample("a"));
        file.set_default("a").unwrap();
        let resolved = resolve_active_policy(None, None, &file).unwrap();
        assert_eq!(resolved.unwrap().name, "a");
    }

    #[test]
    fn returns_none_when_nothing_set() {
        let file = PoliciesFile::default();
        let resolved = resolve_active_policy(None, None, &file).unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn errors_on_unknown_name() {
        let file = PoliciesFile::default();
        let err = resolve_active_policy(Some("missing"), None, &file).unwrap_err();
        assert!(err.to_string().contains("policy `missing` not found"));
    }
}
