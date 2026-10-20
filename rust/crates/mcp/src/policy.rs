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

use crate::context::{ApprovalHold, ApprovalPolicy, peer_supports_elicitation};

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

/// A recorded amount and the ledger day it was charged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub day: u64,
    pub total: u64,
}

/// What a subject has spent today.
pub trait SpendLedger: Send + Sync {
    fn spent_today(&self, subject: &str) -> u64;
    /// Atomically check `cap` and reserve `amount` for today. Returns the
    /// day charged and resulting total, or the would-be total when it
    /// exceeds the cap.
    fn check_and_record(
        &self,
        subject: &str,
        amount: u64,
        cap: Option<u64>,
    ) -> Result<Recorded, u64>;
    /// Release a reservation charged to `day` that did not result in a
    /// signature. A reservation from an earlier day no longer counts against
    /// any cap and is left alone.
    fn release(&self, subject: &str, day: u64, amount: u64);
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

    fn check_and_record(
        &self,
        subject: &str,
        amount: u64,
        cap: Option<u64>,
    ) -> Result<Recorded, u64> {
        let today = (self.clock)();
        let mut totals = self.totals.lock().unwrap();
        let entry = totals.entry(subject.to_string()).or_insert((today, 0));
        if entry.0 != today {
            *entry = (today, 0);
        }
        let next = entry.1.saturating_add(amount);
        if cap.is_some_and(|cap| next > cap) {
            return Err(next);
        }
        entry.1 = next;
        Ok(Recorded {
            day: today,
            total: next,
        })
    }

    fn release(&self, subject: &str, day: u64, amount: u64) {
        let mut totals = self.totals.lock().unwrap();
        if let Some((current, total)) = totals.get_mut(subject)
            && *current == day
        {
            *total = total.saturating_sub(amount);
        }
    }
}

/// The gate itself: refuses over the ceiling or the cap, records what it
/// lets through.
pub struct PolicyGate {
    subject: String,
    policy: SpendPolicy,
    ledger: Arc<dyn SpendLedger>,
    holds: Option<Holds>,
}

/// Reservations a gate recorded for one signing operation.
type Holds = Arc<Mutex<Vec<Reserved>>>;

/// One reservation: what to release if no signature follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserved {
    pub day: u64,
    pub amount: u64,
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
            holds: None,
        }
    }

    fn holding(mut self, holds: Holds) -> Self {
        self.holds = Some(holds);
        self
    }
}

impl PolicyGate {
    /// Check `intent` against the policy and record it. Returns what was
    /// reserved, or `None` for intents that move no money.
    pub fn reserve(&self, intent: &AuthIntent) -> pay_keystore::Result<Option<Reserved>> {
        if !intent.moves_money() {
            return Ok(None);
        }
        let denied = |why: String| pay_keystore::Error::AuthDenied(why);
        if intent.is_recurring() {
            return Err(denied(
                "recurring subscriptions are not supported by the hosted spending policy because future merchant pulls cannot be recorded per request"
                    .to_string(),
            ));
        }
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
        match self
            .ledger
            .check_and_record(&self.subject, amount, self.policy.daily_cap)
        {
            Ok(recorded) => Ok(Some(Reserved {
                day: recorded.day,
                amount,
            })),
            Err(next) => Err(denied(format!(
                "{} would take today's spend to {} against a daily cap of {}",
                dollars(amount),
                dollars(next),
                dollars(self.policy.daily_cap.expect("cap was checked"))
            ))),
        }
    }
}

/// Hosted callers with no interactive prompt: a raw wallet proof has no
/// amount for the policy to bound, so it needs the client's own approval.
struct RequireInteractiveProofApproval;

impl AuthGate for RequireInteractiveProofApproval {
    fn authenticate(&self, intent: &AuthIntent) -> pay_keystore::Result<()> {
        if intent.signs_raw_proof() {
            return Err(pay_keystore::Error::AuthDenied(
                "wallet sign-in needs approval from an MCP client that supports prompts; \
                 this client cannot show one"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

impl AuthGate for PolicyGate {
    fn authenticate(&self, intent: &AuthIntent) -> pay_keystore::Result<()> {
        if let Some(reserved) = self.reserve(intent)?
            && let Some(holds) = &self.holds
        {
            holds.lock().unwrap().push(reserved);
        }
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

impl PolicyApproval {
    fn chain(&self, policy: PolicyGate, peer: Option<&Peer<RoleServer>>) -> AuthOverride {
        let mut gates: Vec<Box<dyn AuthGate>> = vec![Box::new(policy)];
        match peer.filter(|p| peer_supports_elicitation(p)) {
            Some(peer) => gates.push(Box::new(crate::ElicitationAuth::new(peer.clone()))),
            None => gates.push(Box::new(RequireInteractiveProofApproval)),
        }
        Some(Box::new(ChainedGate(gates)))
    }

    fn policy_gate(&self) -> PolicyGate {
        PolicyGate::new(self.subject.clone(), self.policy, self.ledger.clone())
    }
}

impl ApprovalPolicy for PolicyApproval {
    fn gate(&self, peer: Option<&Peer<RoleServer>>) -> AuthOverride {
        self.chain(self.policy_gate(), peer)
    }

    fn gate_with_hold(&self, peer: Option<&Peer<RoleServer>>) -> (AuthOverride, ApprovalHold) {
        let holds = Holds::default();
        let gate = self.chain(self.policy_gate().holding(holds.clone()), peer);
        let (subject, ledger) = (self.subject.clone(), self.ledger.clone());
        let hold = ApprovalHold::new(move || {
            for reserved in std::mem::take(&mut *holds.lock().unwrap()) {
                ledger.release(&subject, reserved.day, reserved.amount);
            }
        });
        (gate, hold)
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
    fn recurring_authority_is_refused_when_future_pulls_cannot_be_recorded() {
        let gate = PolicyGate::new(
            "sub",
            SpendPolicy::dollars(10.0, 100.0),
            Arc::new(MemoryLedger::new()),
        );
        let intent = AuthIntent::authorize_subscription("$1.00", "monthly service", "api.test");

        let error = gate.authenticate(&intent).unwrap_err();
        assert!(
            error.to_string().contains("recurring subscriptions"),
            "{error}"
        );
    }

    #[test]
    fn frozen_lets_nothing_through() {
        let gate = PolicyGate::new("sub", SpendPolicy::FROZEN, Arc::new(MemoryLedger::new()));
        assert!(gate.authenticate(&payment("$0.0001")).is_err());
    }

    #[test]
    fn concurrent_approvals_cannot_exceed_the_daily_cap() {
        let ledger: Arc<dyn SpendLedger> = Arc::new(MemoryLedger::new());
        let policy = SpendPolicy::dollars(1.0, 1.0);
        let gates = [
            PolicyGate::new("sub", policy, ledger.clone()),
            PolicyGate::new("sub", policy, ledger.clone()),
        ];
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = gates.map(|gate| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                gate.authenticate(&payment("$1")).is_ok()
            })
        });
        barrier.wait();
        let approved = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|approved| *approved)
            .count();
        assert_eq!(approved, 1);
        assert_eq!(ledger.spent_today("sub"), 10_000);
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

    fn approval_scope(ledger: Arc<MemoryLedger>) -> crate::CallScope {
        crate::CallScope {
            accounts: Arc::new(pay_core::accounts::MemoryAccountsStore::new()),
            network_override: None,
            account_override: None,
            rpc_url_override: None,
            approval: Arc::new(PolicyApproval {
                subject: "tenant".to_string(),
                policy: SpendPolicy::dollars(1.0, 1.0),
                ledger,
            }),
            payment_permissions: None,
            body_files: false,
            outbound_policy: pay_core::client::fetch::OutboundPolicy::PublicInternetOnly,
        }
    }

    #[test]
    fn declined_or_failed_signing_releases_the_policy_reservation() {
        let ledger = Arc::new(MemoryLedger::new());
        let scope = approval_scope(ledger.clone());

        // The policy approves, then a later gate (the client's prompt)
        // declines or the signer fails: no signature, no spend.
        let result: pay_core::Result<()> = scope.with_approval(None, |gate| {
            gate.expect("hosted policy always gates")
                .authenticate(&payment("$0.50"))
                .map_err(|e| pay_core::Error::Mpp(e.to_string()))?;
            assert_eq!(
                ledger.spent_today("tenant"),
                5_000,
                "reserved while signing"
            );
            Err(pay_core::Error::PaymentRejected("declined".to_string()))
        });
        assert!(result.is_err());
        assert_eq!(ledger.spent_today("tenant"), 0);

        // The full allowance is still available afterwards.
        scope
            .with_approval(None, |gate| {
                gate.unwrap()
                    .authenticate(&payment("$1.00"))
                    .map_err(|e| pay_core::Error::Mpp(e.to_string()))
            })
            .expect("allowance was not consumed by the failure");
        assert_eq!(ledger.spent_today("tenant"), 10_000);
    }

    #[test]
    fn a_refused_chain_releases_what_the_policy_reserved() {
        let ledger = Arc::new(MemoryLedger::new());
        let scope = approval_scope(ledger.clone());
        let result: pay_core::Result<()> = scope.with_approval(None, |gate| {
            let gate = gate.unwrap();
            gate.authenticate(&payment("$0.40"))
                .map_err(|e| pay_core::Error::Mpp(e.to_string()))?;
            // A second authorization in the same operation exceeds the cap.
            gate.authenticate(&payment("$0.70"))
                .map_err(|e| pay_core::Error::Mpp(e.to_string()))
        });
        assert!(result.is_err());
        assert_eq!(ledger.spent_today("tenant"), 0);
    }

    #[test]
    fn a_signed_operation_keeps_its_reservation() {
        let ledger = Arc::new(MemoryLedger::new());
        let scope = approval_scope(ledger.clone());
        let signature = scope
            .with_approval(None, |gate| {
                gate.unwrap()
                    .authenticate(&payment("$0.25"))
                    .map_err(|e| pay_core::Error::Mpp(e.to_string()))?;
                Ok("signed")
            })
            .unwrap();
        assert_eq!(signature, "signed");
        assert_eq!(ledger.spent_today("tenant"), 2_500);
    }

    #[test]
    fn non_payment_operations_reserve_nothing() {
        let ledger = Arc::new(MemoryLedger::new());
        let scope = approval_scope(ledger.clone());
        let _: pay_core::Result<()> = scope.with_approval(None, |gate| {
            gate.unwrap()
                .authenticate(&AuthIntent::use_account("read"))
                .map_err(|e| pay_core::Error::Mpp(e.to_string()))?;
            Err(pay_core::Error::Mpp("failed".to_string()))
        });
        assert_eq!(ledger.spent_today("tenant"), 0);
    }

    #[test]
    fn a_prior_day_release_never_reduces_the_new_day_total() {
        let day = Arc::new(AtomicU64::new(100));
        let ledger = ledger_at(day.clone());
        let policy = SpendPolicy::dollars(10.0, 10.0);
        let gate = PolicyGate::new("sub", policy, ledger.clone());

        let failed = gate.reserve(&payment("$6")).unwrap().unwrap();
        assert_eq!(failed.day, 100);
        day.store(101, Ordering::SeqCst);
        gate.reserve(&payment("$6")).unwrap().unwrap();
        assert_eq!(ledger.spent_today("sub"), 60_000);

        // The day-100 request fails after midnight.
        ledger.release("sub", failed.day, failed.amount);
        assert_eq!(ledger.spent_today("sub"), 60_000);
        assert!(gate.reserve(&payment("$6")).is_err(), "cap still holds");

        // Same-day release still works.
        let same_day = gate.reserve(&payment("$4")).unwrap().unwrap();
        ledger.release("sub", same_day.day, same_day.amount);
        assert_eq!(ledger.spent_today("sub"), 60_000);
    }

    #[test]
    fn approval_hold_across_midnight_leaves_the_new_day_alone() {
        let day = Arc::new(AtomicU64::new(100));
        let ledger = ledger_at(day.clone());
        let approval = PolicyApproval {
            subject: "sub".to_string(),
            policy: SpendPolicy::dollars(10.0, 10.0),
            ledger: ledger.clone(),
        };
        let (gate, hold) = approval.gate_with_hold(None);
        gate.unwrap().authenticate(&payment("$6")).unwrap();
        day.store(101, Ordering::SeqCst);
        PolicyGate::new("sub", approval.policy, ledger.clone())
            .authenticate(&payment("$6"))
            .unwrap();
        drop(hold);
        assert_eq!(ledger.spent_today("sub"), 60_000);
    }

    #[test]
    fn hosted_sign_in_without_a_client_prompt_is_refused() {
        let ledger = Arc::new(MemoryLedger::new());
        let approval = PolicyApproval {
            subject: "sub".to_string(),
            policy: SpendPolicy::FROZEN,
            ledger: ledger.clone(),
        };
        let gate = approval.gate(None).expect("hosted policy always gates");
        let err = gate
            .authenticate(&AuthIntent::sign_in("sign in to merchant.example"))
            .unwrap_err();
        assert!(
            err.to_string().contains("wallet sign-in needs approval"),
            "{err}"
        );
        assert_eq!(ledger.spent_today("sub"), 0);

        // Plain account use and in-policy payments still pass without a prompt.
        let approval = PolicyApproval {
            subject: "sub".to_string(),
            policy: SpendPolicy::dollars(1.0, 1.0),
            ledger: ledger.clone(),
        };
        let gate = approval.gate(None).unwrap();
        assert!(gate.authenticate(&AuthIntent::use_account("read")).is_ok());
        assert!(gate.authenticate(&payment("$0.10")).is_ok());
    }

    /// A throwaway file-backed `connector` account on mainnet that requires
    /// approval, so the supplied gate runs inside signer resolution.
    fn gated_file_store(dir: &tempfile::TempDir) -> pay_core::accounts::MemoryAccountsStore {
        const KEYPAIR: [u8; 64] = [
            41, 99, 180, 88, 51, 57, 48, 80, 61, 63, 219, 75, 176, 49, 116, 254, 227, 176, 196,
            204, 122, 47, 166, 133, 155, 252, 217, 0, 253, 17, 49, 143, 47, 94, 121, 167, 195, 136,
            72, 22, 157, 48, 77, 88, 63, 96, 57, 122, 181, 243, 236, 188, 241, 134, 174, 224, 100,
            246, 17, 170, 104, 17, 151, 48,
        ];
        let path = dir.path().join("connector.json");
        std::fs::write(&path, serde_json::to_string(&KEYPAIR.to_vec()).unwrap()).unwrap();
        let mut file = pay_core::accounts::AccountsFile::default();
        file.upsert(
            "mainnet",
            "connector",
            pay_core::accounts::Account {
                backend: pay_core::accounts::BackendKind::File,
                provider: None,
                active: true,
                auth_required: Some(true),
                pubkey: Some("4BuiY9QUUfPoAGNJBja3JapAuVWMc9c7in6UCgyC2zPR".to_string()),
                vault: None,
                account: None,
                path: Some(path.to_string_lossy().into_owned()),
                secret_key_b58: None,
                created_at: None,
                subscriptions: Default::default(),
            },
        );
        pay_core::accounts::MemoryAccountsStore::with_file(file)
    }

    #[test]
    fn hosted_siwx_header_is_not_signed_without_a_client_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let store = gated_file_store(&dir);
        let challenge = pay_core::client::x402::SiwxAuthChallenge {
            extension: pay_kit::x402::siwx::SiwxExtension::new(
                pay_kit::x402::siwx::SiwxExtensionInfo {
                    domain: "merchant.example".to_string(),
                    uri: "https://merchant.example/data".to_string(),
                    statement: None,
                    version: "1".to_string(),
                    nonce: "nonce".to_string(),
                    issued_at: "2026-10-19T00:00:00Z".to_string(),
                    expiration_time: None,
                    not_before: None,
                    request_id: None,
                    resources: None,
                },
                pay_kit::x402::siwx::default_solana_siwx_chains(),
            ),
        };
        let approval = PolicyApproval {
            subject: "sub".to_string(),
            policy: SpendPolicy::dollars(1.0, 10.0),
            ledger: Arc::new(MemoryLedger::new()),
        };
        let result = pay_core::client::x402::build_siwx_auth_header_with_override(
            &challenge,
            &store,
            Some("mainnet"),
            Some("connector"),
            Some("https://merchant.example/data"),
            approval.gate(None),
        );
        // The same store signs once a gate approves (see the core SIWX tests);
        // here the hosted gate refuses the raw proof.
        let err = result.expect_err("raw proof signed without approval");
        assert!(matches!(err, pay_core::Error::PaymentRejected(_)), "{err}");
    }

    #[test]
    fn native_sol_x402_challenge_is_not_authorized_as_dollars() {
        let dir = tempfile::tempdir().unwrap();
        let store = gated_file_store(&dir);
        let ledger = Arc::new(MemoryLedger::new());
        let gate = PolicyGate::new("tenant", SpendPolicy::dollars(1.0, 10.0), ledger.clone());
        let requirements: pay_kit::x402::exact::PaymentRequirements =
            serde_json::from_value(serde_json::json!({
                "network": "mainnet",
                "cluster": "mainnet",
                "recipient": "11111111111111111111111111111111",
                "amount": "1000000000",
                "currency": "SOL",
                "decimals": 9,
                "resource": "https://attacker.example/paid",
                "recentBlockhash": "11111111111111111111111111111111",
            }))
            .unwrap();
        let challenge = pay_core::client::x402::Challenge {
            x402_version: 2,
            requirements,
            siwx: None,
            extensions: None,
        };

        let result = pay_core::client::x402::build_payment_with_override(
            &challenge,
            &store,
            Some("mainnet"),
            Some("connector"),
            Some("https://attacker.example/paid"),
            Some(Box::new(gate)),
        );
        let err = result.expect_err("1 SOL must not pass a $1 policy");
        assert!(matches!(err, pay_core::Error::PaymentRejected(_)), "{err}");
        assert_eq!(ledger.spent_today("tenant"), 0, "nothing recorded as USD");

        // Control: the same store and policy sign an in-policy USDC payment.
        pay_core::test_rpc::install_blockhash_rpc();
        let gate = PolicyGate::new("tenant", SpendPolicy::dollars(1.0, 10.0), ledger.clone());
        let requirements: pay_kit::x402::exact::PaymentRequirements =
            serde_json::from_value(serde_json::json!({
                "network": "mainnet",
                "cluster": "mainnet",
                "recipient": "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY",
                "amount": "500000",
                "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "decimals": 6,
                "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "resource": "https://api.example/paid",
                "recentBlockhash": "11111111111111111111111111111111",
            }))
            .unwrap();
        pay_core::client::x402::build_payment_with_override(
            &pay_core::client::x402::Challenge {
                x402_version: 2,
                requirements,
                siwx: None,
                extensions: None,
            },
            &store,
            Some("mainnet"),
            Some("connector"),
            Some("https://api.example/paid"),
            Some(Box::new(gate)),
        )
        .expect("in-policy stablecoin payment signs");
        assert_eq!(ledger.spent_today("tenant"), 5_000);
    }

    #[test]
    fn native_sol_intent_names_sol_and_carries_no_usd_value() {
        let intent = AuthIntent::authorize_payment_details("1 SOL", "api", "attacker.example");
        assert_eq!(intent.amount_minor_units(), None);
        assert_eq!(intent.payment_limit(), None);
        assert!(intent.message().contains("1 SOL"));
        let gate = PolicyGate::new(
            "sub",
            SpendPolicy::dollars(100.0, 100.0),
            Arc::new(MemoryLedger::new()),
        );
        let err = gate.authenticate(&intent).unwrap_err();
        assert!(
            err.to_string().contains("does not state an amount"),
            "{err}"
        );
    }

    fn mpp_charge(currency: &str, amount: &str) -> pay_core::client::mpp::Challenge {
        let request = serde_json::json!({
            "amount": amount,
            "currency": currency,
            "recipient": "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY",
            "methodDetails": {
                "network": "mainnet",
                "recentBlockhash": "11111111111111111111111111111111",
            },
        });
        pay_kit::mpp::PaymentChallenge::new(
            "id",
            "realm",
            "solana",
            "charge",
            pay_kit::mpp::Base64UrlJson::from_value(&request).unwrap(),
        )
    }

    #[test]
    fn native_sol_mpp_charge_is_refused_by_the_usd_policy_before_signing() {
        let dir = tempfile::tempdir().unwrap();
        let store = gated_file_store(&dir);
        let ledger = Arc::new(MemoryLedger::new());
        let gate = PolicyGate::new("tenant", SpendPolicy::dollars(1.0, 10.0), ledger.clone());
        let result = pay_core::client::mpp::build_credential_with_override(
            &mpp_charge("SOL", "1000000000"),
            &store,
            Some("mainnet"),
            Some("connector"),
            Some("https://attacker.example/paid"),
            Some(Box::new(gate)),
        );
        let err = result.expect_err("1 SOL must not pass a $1 policy");
        assert!(matches!(err, pay_core::Error::PaymentRejected(_)), "{err}");
        assert_eq!(ledger.spent_today("tenant"), 0);
    }

    #[test]
    fn hosted_permissions_refuse_native_sol_before_any_payment_is_built() {
        let hosted = crate::McpPermissions::hosted(&crate::ApprovedRecipients::Any);
        assert!(
            hosted
                .authorize_mpp(&mpp_charge("SOL", "1000000000"), "https://a.example/x")
                .is_err()
        );
        assert!(
            hosted
                .authorize_mpp(&mpp_charge("USDC", "1000000"), "https://a.example/x")
                .is_ok()
        );
    }

    #[test]
    fn a_non_six_decimal_token_is_not_valued_as_six_decimal_dollars() {
        let dir = tempfile::tempdir().unwrap();
        let store = gated_file_store(&dir);
        let ledger = Arc::new(MemoryLedger::new());
        let gate = PolicyGate::new("tenant", SpendPolicy::dollars(100.0, 100.0), ledger.clone());
        let requirements: pay_kit::x402::exact::PaymentRequirements =
            serde_json::from_value(serde_json::json!({
                "network": "mainnet",
                "cluster": "mainnet",
                "recipient": "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY",
                "amount": "50000000",
                "currency": "Mint111111111111111111111111111111111111111",
                "decimals": 2,
                "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "resource": "https://attacker.example/x402",
                "recentBlockhash": "11111111111111111111111111111111",
            }))
            .unwrap();
        let result = pay_core::client::x402::build_payment_with_override(
            &pay_core::client::x402::Challenge {
                x402_version: 2,
                requirements,
                siwx: None,
                extensions: None,
            },
            &store,
            Some("mainnet"),
            Some("connector"),
            Some("https://attacker.example/x402"),
            Some(Box::new(gate)),
        );
        // 500,000 whole tokens used to be authorized as $50.
        let err = result.expect_err("custom-mint amount has no USD value");
        assert!(matches!(err, pay_core::Error::PaymentRejected(_)), "{err}");
        assert_eq!(ledger.spent_today("tenant"), 0);
    }
}
