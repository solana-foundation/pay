//! Spending policy as an approval gate.
//!
//! A hosted tenant has no Touch ID. What stands in for it is a policy: a
//! ceiling per call and a cap per day, checked against the intent's amount
//! before a signature is released and recorded when it is. When the MCP
//! client can also show a prompt, the policy runs first and the prompt
//! second: a cap is a hard limit, a prompt is a courtesy.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pay_core::keystore::{AuthGate, AuthIntent, USD_MINOR_UNITS_PER_DOLLAR};
use pay_core::signer::AuthOverride;
use rmcp::service::{Peer, RoleServer};

use crate::context::{ApprovalPolicy, peer_supports_elicitation};

/// Limits in USD ten-thousandths ([`USD_MINOR_UNITS_PER_DOLLAR`]).
/// `None` means no limit of that kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendPolicy {
    pub per_call_ceiling: Option<u64>,
    pub daily_cap: Option<u64>,
}

impl SpendPolicy {
    /// Nothing may move. The state of a tenant who has not set limits, and
    /// the way to freeze one without touching funds.
    pub const FROZEN: Self = Self {
        per_call_ceiling: Some(0),
        daily_cap: Some(0),
    };

    pub fn dollars(per_call: f64, daily: f64) -> Self {
        let to_minor = |d: f64| (d * USD_MINOR_UNITS_PER_DOLLAR as f64).round().max(0.0) as u64;
        Self {
            per_call_ceiling: Some(to_minor(per_call)),
            daily_cap: Some(to_minor(daily)),
        }
    }
}

fn dollars(minor: u64) -> String {
    let whole = minor / USD_MINOR_UNITS_PER_DOLLAR;
    let frac = minor % USD_MINOR_UNITS_PER_DOLLAR;
    if frac == 0 {
        format!("${whole}")
    } else {
        format!("${whole}.{frac:04}")
            .trim_end_matches('0')
            .to_string()
    }
}

/// What a subject has spent today.
pub trait SpendLedger: Send + Sync {
    fn spent_today(&self, subject: &str) -> u64;
    fn record(&self, subject: &str, amount: u64);
}

/// Day counter, so a ledger can be driven in tests.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn utc_day_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// In-memory ledger, one running total per subject per UTC day.
pub struct MemoryLedger {
    totals: Mutex<HashMap<String, (u64, u64)>>,
    clock: Clock,
}

impl Default for MemoryLedger {
    fn default() -> Self {
        Self::with_clock(Arc::new(utc_day_now))
    }
}

impl MemoryLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_clock(clock: Clock) -> Self {
        Self {
            totals: Mutex::default(),
            clock,
        }
    }
}

impl SpendLedger for MemoryLedger {
    fn spent_today(&self, subject: &str) -> u64 {
        let today = (self.clock)();
        match self.totals.lock().unwrap().get(subject) {
            Some((day, total)) if *day == today => *total,
            _ => 0,
        }
    }

    fn record(&self, subject: &str, amount: u64) {
        let today = (self.clock)();
        let mut totals = self.totals.lock().unwrap();
        let entry = totals.entry(subject.to_string()).or_insert((today, 0));
        if entry.0 != today {
            *entry = (today, 0);
        }
        entry.1 = entry.1.saturating_add(amount);
    }
}

/// The gate itself: refuses over the ceiling or the cap, records what it
/// lets through.
pub struct PolicyGate {
    subject: String,
    policy: SpendPolicy,
    ledger: Arc<dyn SpendLedger>,
}

impl PolicyGate {
    pub fn new(
        subject: impl Into<String>,
        policy: SpendPolicy,
        ledger: Arc<dyn SpendLedger>,
    ) -> Self {
        Self {
            subject: subject.into(),
            policy,
            ledger,
        }
    }
}

impl AuthGate for PolicyGate {
    fn authenticate(&self, intent: &AuthIntent) -> pay_keystore::Result<()> {
        if !intent.moves_money() {
            return Ok(());
        }
        let denied = |why: String| pay_keystore::Error::AuthDenied(why);
        let Some(amount) = intent.amount_minor_units() else {
            return Err(denied(
                "this payment does not state an amount, and the account is under a spending \
                 policy that needs one"
                    .to_string(),
            ));
        };
        if let Some(ceiling) = self.policy.per_call_ceiling
            && amount > ceiling
        {
            return Err(denied(format!(
                "{} exceeds the per-call ceiling of {}",
                dollars(amount),
                dollars(ceiling)
            )));
        }
        if let Some(cap) = self.policy.daily_cap {
            let spent = self.ledger.spent_today(&self.subject);
            if spent.saturating_add(amount) > cap {
                return Err(denied(format!(
                    "{} would take today's spend to {} against a daily cap of {}",
                    dollars(amount),
                    dollars(spent.saturating_add(amount)),
                    dollars(cap)
                )));
            }
        }
        // Recorded at approval, before the payment settles: a burst of
        // calls cannot slip past the cap between check and settlement.
        self.ledger.record(&self.subject, amount);
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// Run gates in order; the first refusal wins.
pub struct ChainedGate(pub Vec<Box<dyn AuthGate>>);

impl AuthGate for ChainedGate {
    fn authenticate(&self, intent: &AuthIntent) -> pay_keystore::Result<()> {
        self.0.iter().try_for_each(|g| g.authenticate(intent))
    }
    fn is_available(&self) -> bool {
        self.0.iter().all(|g| g.is_available())
    }
}

/// A tenant's approval: the policy always, plus the client's own prompt
/// when it has one.
pub struct PolicyApproval {
    pub subject: String,
    pub policy: SpendPolicy,
    pub ledger: Arc<dyn SpendLedger>,
}

impl ApprovalPolicy for PolicyApproval {
    fn gate(&self, peer: Option<&Peer<RoleServer>>) -> AuthOverride {
        let mut gates: Vec<Box<dyn AuthGate>> = vec![Box::new(PolicyGate::new(
            self.subject.clone(),
            self.policy,
            self.ledger.clone(),
        ))];
        if let Some(peer) = peer.filter(|p| peer_supports_elicitation(p)) {
            gates.push(Box::new(crate::ElicitationAuth::new(peer.clone())));
        }
        Some(Box::new(ChainedGate(gates)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn ledger_at(day: Arc<AtomicU64>) -> Arc<MemoryLedger> {
        Arc::new(MemoryLedger::with_clock(Arc::new(move || {
            day.load(Ordering::SeqCst)
        })))
    }

    fn payment(amount: &str) -> AuthIntent {
        AuthIntent::authorize_payment_details(amount, "test", "api.test")
    }

    #[test]
    fn dollars_formats_minor_units() {
        assert_eq!(dollars(10_000), "$1");
        assert_eq!(dollars(450), "$0.045");
        assert_eq!(dollars(250_000), "$25");
        assert_eq!(dollars(12_345), "$1.2345");
        assert_eq!(
            SpendPolicy::dollars(0.05, 5.0),
            SpendPolicy {
                per_call_ceiling: Some(500),
                daily_cap: Some(50_000)
            }
        );
    }

    #[test]
    fn ceiling_cap_and_recording() {
        let day = Arc::new(AtomicU64::new(100));
        let ledger = ledger_at(day.clone());
        let gate = PolicyGate::new("sub_1", SpendPolicy::dollars(1.0, 2.5), ledger.clone());

        assert!(gate.authenticate(&payment("$1.00")).is_ok());
        assert_eq!(ledger.spent_today("sub_1"), 10_000);

        let err = gate.authenticate(&payment("$1.01")).unwrap_err();
        assert!(err.to_string().contains("per-call ceiling of $1"), "{err}");

        assert!(gate.authenticate(&payment("$1.00")).is_ok());
        let err = gate.authenticate(&payment("$1.00")).unwrap_err();
        assert!(err.to_string().contains("daily cap of $2.5"), "{err}");
        assert_eq!(
            ledger.spent_today("sub_1"),
            20_000,
            "a refusal records nothing"
        );

        // Another subject has its own day; a new day resets.
        assert_eq!(ledger.spent_today("sub_2"), 0);
        day.store(101, Ordering::SeqCst);
        assert_eq!(ledger.spent_today("sub_1"), 0);
        assert!(gate.authenticate(&payment("$1.00")).is_ok());
    }

    #[test]
    fn unpriced_payments_are_refused_and_non_payments_pass() {
        let gate = PolicyGate::new(
            "sub",
            SpendPolicy::dollars(1.0, 1.0),
            Arc::new(MemoryLedger::new()),
        );
        let err = gate
            .authenticate(&AuthIntent::default_payment())
            .unwrap_err();
        assert!(
            err.to_string().contains("does not state an amount"),
            "{err}"
        );
        assert!(gate.authenticate(&AuthIntent::create_account("a")).is_ok());
        assert!(gate.authenticate(&AuthIntent::use_account("read")).is_ok());
    }

    #[test]
    fn frozen_lets_nothing_through() {
        let gate = PolicyGate::new("sub", SpendPolicy::FROZEN, Arc::new(MemoryLedger::new()));
        assert!(gate.authenticate(&payment("$0.0001")).is_err());
    }

    #[test]
    fn chained_gate_stops_at_the_first_refusal() {
        struct Fixed(bool);
        impl AuthGate for Fixed {
            fn authenticate(&self, _: &AuthIntent) -> pay_keystore::Result<()> {
                if self.0 {
                    Ok(())
                } else {
                    Err(pay_keystore::Error::AuthDenied("no".into()))
                }
            }
            fn is_available(&self) -> bool {
                true
            }
        }
        assert!(
            ChainedGate(vec![Box::new(Fixed(true)), Box::new(Fixed(true))])
                .authenticate(&payment("$1"))
                .is_ok()
        );
        assert!(
            ChainedGate(vec![Box::new(Fixed(true)), Box::new(Fixed(false))])
                .authenticate(&payment("$1"))
                .is_err()
        );
        assert!(ChainedGate(vec![]).authenticate(&payment("$1")).is_ok());
    }
}
