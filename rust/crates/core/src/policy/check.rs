//! Pure policy-check logic. No I/O — call this from the 402 builders after
//! loading state and before loading the signer.

use chrono::{DateTime, Duration, Utc};

use super::config::Policy;
use super::state::PerPolicyState;

const ONE_DAY: Duration = Duration::seconds(86_400);

/// Why a policy rejected a payment. Carries enough detail to render a
/// helpful CLI error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyViolation {
    Paused,
    Expired { expired_at: DateTime<Utc> },
    ExceedsPerTx { amount: u64, max: u64 },
    ExceedsDailyCap { spent: u64, amount: u64, cap: u64 },
    RecipientNotAllowed { recipient: String },
    OriginNotAllowed { origin: String },
    /// Both lists are set but the request matched neither.
    NoAllowlistMatch {
        recipient: Option<String>,
        origin: Option<String>,
    },
}

impl PolicyViolation {
    /// One-line user-facing message for the CLI / MCP error response.
    pub fn user_message(&self) -> String {
        match self {
            Self::Paused => "policy is paused".to_string(),
            Self::Expired { expired_at } => {
                format!("policy expired at {}", expired_at.to_rfc3339())
            }
            Self::ExceedsPerTx { amount, max } => format!(
                "amount {} exceeds per-tx cap {} (micro-USDC)",
                amount, max
            ),
            Self::ExceedsDailyCap {
                spent,
                amount,
                cap,
            } => format!(
                "daily cap exceeded: would spend {} of {} (already spent {}, all micro-USDC)",
                spent + amount,
                cap,
                spent,
            ),
            Self::RecipientNotAllowed { recipient } => {
                format!("recipient {recipient} is not in policy allowlist")
            }
            Self::OriginNotAllowed { origin } => {
                format!("request origin {origin} is not in policy allowlist")
            }
            Self::NoAllowlistMatch { recipient, origin } => format!(
                "neither recipient ({}) nor origin ({}) matches policy allowlists",
                recipient.as_deref().unwrap_or("?"),
                origin.as_deref().unwrap_or("?"),
            ),
        }
    }
}

/// Run the policy gate for a single about-to-be-signed payment.
///
/// On success, the daily-window roll has happened in-place on `state` (if
/// applicable) and the caller should persist `state` if it was mutated. On
/// failure, `state` is unchanged.
///
/// Both allowlist semantics:
/// - Both empty ⇒ no allowlist restriction.
/// - One non-empty ⇒ that list must match.
/// - Both non-empty ⇒ either match passes.
pub fn check_payment(
    policy: &Policy,
    state: &mut PerPolicyState,
    amount: u64,
    recipient: &str,
    request_origin: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), PolicyViolation> {
    if policy.paused {
        return Err(PolicyViolation::Paused);
    }
    if let Some(expires_at) = policy.expires_at
        && now >= expires_at
    {
        return Err(PolicyViolation::Expired { expired_at: expires_at });
    }
    if amount > policy.max_per_tx {
        return Err(PolicyViolation::ExceedsPerTx {
            amount,
            max: policy.max_per_tx,
        });
    }

    // Daily-window roll: reset if the existing window is >= 24h old, OR if
    // there's no recorded window yet (first use).
    let needs_reset = match state.day_reset_ts {
        Some(ts) => now - ts >= ONE_DAY,
        None => true,
    };
    if needs_reset {
        state.spent_today = 0;
        state.day_reset_ts = Some(now);
    }

    let new_total = state
        .spent_today
        .checked_add(amount)
        .ok_or(PolicyViolation::ExceedsDailyCap {
            spent: state.spent_today,
            amount,
            cap: policy.daily_cap,
        })?;
    if new_total > policy.daily_cap {
        return Err(PolicyViolation::ExceedsDailyCap {
            spent: state.spent_today,
            amount,
            cap: policy.daily_cap,
        });
    }

    // Allowlist check.
    let recipients_set = !policy.allowed_recipients.is_empty();
    let origins_set = !policy.allowed_origins.is_empty();
    if recipients_set || origins_set {
        let recipient_match =
            recipients_set && policy.allowed_recipients.iter().any(|r| r == recipient);
        let origin_match = origins_set
            && request_origin.is_some_and(|o| policy.allowed_origins.iter().any(|a| a == o));

        if !recipient_match && !origin_match {
            // Pick the most specific error.
            return Err(match (recipients_set, origins_set) {
                (true, false) => PolicyViolation::RecipientNotAllowed {
                    recipient: recipient.to_string(),
                },
                (false, true) => PolicyViolation::OriginNotAllowed {
                    origin: request_origin.unwrap_or("(none)").to_string(),
                },
                _ => PolicyViolation::NoAllowlistMatch {
                    recipient: Some(recipient.to_string()),
                    origin: request_origin.map(str::to_string),
                },
            });
        }
    }

    Ok(())
}

/// Increment `spent_today` and update `last_paid_at` after a successful
/// HTTP retry. Caller persists the state file.
pub fn record_payment(state: &mut PerPolicyState, amount: u64, now: DateTime<Utc>) {
    state.spent_today = state.spent_today.saturating_add(amount);
    state.last_paid_at = Some(now);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn base_policy() -> Policy {
        Policy {
            name: "test".to_string(),
            max_per_tx: 100_000,
            daily_cap: 1_000_000,
            allowed_recipients: vec![],
            allowed_origins: vec![],
            expires_at: None,
            paused: false,
            created_at: t("2026-01-01T00:00:00Z"),
        }
    }

    #[test]
    fn happy_path() {
        let policy = base_policy();
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            50_000,
            "RecipientPubkey",
            Some("api.example.com"),
            t("2026-05-01T12:00:00Z"),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn paused_blocks() {
        let mut policy = base_policy();
        policy.paused = true;
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "X",
            None,
            t("2026-05-01T12:00:00Z"),
        );
        assert_eq!(result, Err(PolicyViolation::Paused));
    }

    #[test]
    fn expired_blocks() {
        let mut policy = base_policy();
        policy.expires_at = Some(t("2026-04-01T00:00:00Z"));
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "X",
            None,
            t("2026-05-01T12:00:00Z"),
        );
        assert!(matches!(result, Err(PolicyViolation::Expired { .. })));
    }

    #[test]
    fn exceeds_per_tx() {
        let policy = base_policy();
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            100_001,
            "X",
            None,
            t("2026-05-01T12:00:00Z"),
        );
        assert_eq!(
            result,
            Err(PolicyViolation::ExceedsPerTx {
                amount: 100_001,
                max: 100_000,
            })
        );
    }

    #[test]
    fn exceeds_daily_cap() {
        let policy = base_policy();
        let mut state = PerPolicyState {
            spent_today: 950_000,
            day_reset_ts: Some(t("2026-05-01T00:00:00Z")),
            last_paid_at: None,
        };
        let result = check_payment(
            &policy,
            &mut state,
            100_000,
            "X",
            None,
            t("2026-05-01T12:00:00Z"),
        );
        assert_eq!(
            result,
            Err(PolicyViolation::ExceedsDailyCap {
                spent: 950_000,
                amount: 100_000,
                cap: 1_000_000,
            })
        );
    }

    #[test]
    fn daily_window_resets_after_24h() {
        let policy = base_policy();
        let mut state = PerPolicyState {
            spent_today: 950_000,
            day_reset_ts: Some(t("2026-05-01T00:00:00Z")),
            last_paid_at: None,
        };
        // 24h after the window opened, the counter should reset and the
        // amount should now fit.
        let now = t("2026-05-02T00:00:01Z");
        let result = check_payment(&policy, &mut state, 100_000, "X", None, now);
        assert!(result.is_ok());
        assert_eq!(state.spent_today, 0); // reset happens before the (deferred) increment
        assert_eq!(state.day_reset_ts, Some(now));
    }

    #[test]
    fn first_use_seeds_window() {
        let policy = base_policy();
        let mut state = PerPolicyState::default();
        let now = t("2026-05-01T12:00:00Z");
        let result = check_payment(&policy, &mut state, 10, "X", None, now);
        assert!(result.is_ok());
        assert_eq!(state.day_reset_ts, Some(now));
    }

    #[test]
    fn recipient_allowlist_hit() {
        let mut policy = base_policy();
        policy.allowed_recipients = vec!["GoodRecipient".to_string()];
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "GoodRecipient",
            None,
            t("2026-05-01T12:00:00Z"),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn recipient_allowlist_miss() {
        let mut policy = base_policy();
        policy.allowed_recipients = vec!["GoodRecipient".to_string()];
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "OtherRecipient",
            None,
            t("2026-05-01T12:00:00Z"),
        );
        assert_eq!(
            result,
            Err(PolicyViolation::RecipientNotAllowed {
                recipient: "OtherRecipient".to_string(),
            })
        );
    }

    #[test]
    fn origin_allowlist_hit() {
        let mut policy = base_policy();
        policy.allowed_origins = vec!["api.example.com".to_string()];
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "X",
            Some("api.example.com"),
            t("2026-05-01T12:00:00Z"),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn origin_allowlist_miss() {
        let mut policy = base_policy();
        policy.allowed_origins = vec!["api.example.com".to_string()];
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "X",
            Some("other.example.com"),
            t("2026-05-01T12:00:00Z"),
        );
        assert_eq!(
            result,
            Err(PolicyViolation::OriginNotAllowed {
                origin: "other.example.com".to_string(),
            })
        );
    }

    #[test]
    fn both_allowlists_or_semantics_either_passes() {
        let mut policy = base_policy();
        policy.allowed_recipients = vec!["GoodRecipient".to_string()];
        policy.allowed_origins = vec!["api.example.com".to_string()];

        // Recipient matches, origin doesn't → pass (OR).
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "GoodRecipient",
            Some("other.example.com"),
            t("2026-05-01T12:00:00Z"),
        );
        assert!(result.is_ok());

        // Origin matches, recipient doesn't → pass (OR).
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "OtherRecipient",
            Some("api.example.com"),
            t("2026-05-01T12:00:00Z"),
        );
        assert!(result.is_ok());

        // Neither matches → reject with NoAllowlistMatch.
        let mut state = PerPolicyState::default();
        let result = check_payment(
            &policy,
            &mut state,
            10,
            "OtherRecipient",
            Some("other.example.com"),
            t("2026-05-01T12:00:00Z"),
        );
        assert!(matches!(
            result,
            Err(PolicyViolation::NoAllowlistMatch { .. })
        ));
    }

    #[test]
    fn record_payment_increments_and_timestamps() {
        let mut state = PerPolicyState::default();
        let now = t("2026-05-01T12:00:00Z");
        record_payment(&mut state, 50_000, now);
        assert_eq!(state.spent_today, 50_000);
        assert_eq!(state.last_paid_at, Some(now));

        record_payment(&mut state, 25_000, now);
        assert_eq!(state.spent_today, 75_000);
    }

    #[test]
    fn record_payment_saturates_on_overflow() {
        let mut state = PerPolicyState {
            spent_today: u64::MAX - 5,
            day_reset_ts: None,
            last_paid_at: None,
        };
        record_payment(&mut state, 100, t("2026-05-01T12:00:00Z"));
        assert_eq!(state.spent_today, u64::MAX);
    }

    #[test]
    fn rejection_does_not_mutate_state_for_paused() {
        let mut policy = base_policy();
        policy.paused = true;
        let mut state = PerPolicyState {
            spent_today: 500_000,
            day_reset_ts: Some(t("2026-04-01T00:00:00Z")),
            last_paid_at: None,
        };
        let snapshot = state.clone();
        let _ = check_payment(
            &policy,
            &mut state,
            10,
            "X",
            None,
            t("2026-05-01T12:00:00Z"),
        );
        // Day-roll should not happen if paused (we bail before).
        assert_eq!(state.spent_today, snapshot.spent_today);
        assert_eq!(state.day_reset_ts, snapshot.day_reset_ts);
    }

    #[test]
    fn user_messages_are_human_readable() {
        let messages = [
            PolicyViolation::Paused.user_message(),
            PolicyViolation::ExceedsPerTx {
                amount: 1,
                max: 0,
            }
            .user_message(),
            PolicyViolation::ExceedsDailyCap {
                spent: 1,
                amount: 1,
                cap: 1,
            }
            .user_message(),
        ];
        for m in messages {
            assert!(!m.is_empty());
            assert!(m.is_ascii());
        }
    }
}
