//! Optional spending-policy consultation, not signature custody or controlled egress.

use pay_core::client::fetch::RequestBody;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Frozen application request, before any payment credential is attached.
/// Contains potentially sensitive application input: implementations must not log it.
#[derive(Clone, Serialize)]
pub struct ImmutableRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// Facts obtained from the real SDK-decoded x402 exact offer.
#[derive(Clone, Serialize)]
pub struct SpendRequest {
    pub operation_id: String,
    /// Unique to this execution attempt, even across MCP process restarts.
    pub attempt_id: String,
    /// Asserted by the trusted PayContext, never by tool arguments/clientInfo.
    pub caller_id: String,
    pub request: ImmutableRequest,
    pub amount_atomic: String,
    pub asset: String,
    pub network: String,
    pub execution_network: String,
    pub recipient: String,
    /// Full canonical accepted offer, including decimals/fee payer/memo/blockhash.
    pub accepted: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityDecision {
    Allow,
    Deny,
    /// Provider guarantees rejection before any durable/economic side effect.
    /// Only the exact same caller/operation/immutable request may retry.
    RejectBeforeExecution,
}

/// Provider failures are deliberately opaque to the agent (no raw transport errors).
#[derive(Debug)]
pub struct AuthorityUnavailable;

/// Runtime policy provider. It must not block its future's executor thread.
///
/// Providers requiring durable idempotency must atomically bind caller + operation
/// to immutable request/offer facts and one attempt_id. A new attempt for an already
/// claimed operation must fail closed unless its prior attempt explicitly returned
/// RejectBeforeExecution with no durable/economic side effects. Repeating an allow
/// without this check does NOT provide at-most-once execution across process restarts.
pub trait SpendingAuthority: Send + Sync {
    fn authorize(
        &self,
        request: SpendRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AuthorityDecision, AuthorityUnavailable>> + Send + '_>>;
}

/// Trusted, per-caller configuration supplied by PayContext. It is additive to YAML
/// permissions and wallet approval, and cannot be configured via a tool request.
#[derive(Clone)]
pub struct AuthorityBinding {
    pub(crate) caller_id: String,
    authority: Arc<dyn SpendingAuthority>,
    timeout: Duration,
    controlled: Option<Arc<dyn crate::controlled::ControlledExecution>>,
}

impl AuthorityBinding {
    pub fn new(caller_id: impl Into<String>, authority: Arc<dyn SpendingAuthority>) -> Self {
        Self {
            caller_id: caller_id.into(),
            authority,
            timeout: Duration::from_secs(5),
            controlled: None,
        }
    }

    /// Opt in to host custody after authorization. Pay prepares only unsigned
    /// bytes; the controller owns signing, final HTTP and original-result recovery.
    pub fn with_controlled_execution(
        mut self,
        controller: Arc<dyn crate::controlled::ControlledExecution>,
    ) -> Self {
        self.controlled = Some(controller);
        self
    }

    /// Invalid (zero or >60s) deadlines fail closed when the call starts.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

// A bounded connection-local replay guard, not an economic ledger. Entries never
// expire/evict: a failed/unknown attempt must not silently become a fresh payment.
#[derive(Clone, Default)]
pub(crate) struct Operations(Arc<Mutex<HashMap<(String, String), Operation>>>);

struct Operation {
    digest: [u8; 32],
    state: OperationState,
}

// Claimed includes in-flight and all terminal/ambiguous outcomes. No drop handler
// releases it: cancellation, panic and lost responses must remain fenced.
enum OperationState {
    Claimed(String),
    Retryable,
}

pub(crate) struct AuthorityAttempt {
    operations: Operations,
    binding: AuthorityBinding,
    operation_id: String,
    attempt_id: String,
    request: ImmutableRequest,
}

fn rejected(code: &str) -> pay_core::Error {
    pay_core::Error::PaymentRejected(format!("External spending authority: {code}"))
}

impl Operations {
    pub(crate) fn start(
        &self,
        binding: AuthorityBinding,
        operation_id: Option<&str>,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&RequestBody>,
    ) -> Result<AuthorityAttempt, pay_core::Error> {
        let id = operation_id
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
            })
            .ok_or_else(|| rejected("OPERATION_ID_REQUIRED"))?;
        if binding.caller_id.trim().is_empty()
            || binding.caller_id.len() > 256
            || binding.timeout.is_zero()
            || binding.timeout > Duration::from_secs(60)
        {
            return Err(rejected("INVALID_TRUSTED_CONFIGURATION"));
        }
        let parsed_url = reqwest::Url::parse(url).map_err(|_| rejected("INVALID_REQUEST"))?;
        // Reqwest turns URL userinfo into Authorization before sending the probe.
        if !parsed_url.username().is_empty() || parsed_url.password().is_some() {
            return Err(rejected("PREFILLED_CREDENTIAL_OR_AMBIGUOUS_HEADER"));
        }
        let mut canonical_headers = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect::<Vec<_>>();
        canonical_headers.sort();
        if canonical_headers.iter().any(|(name, _)| {
            matches!(
                name.as_str(),
                "authorization"
                    | "cookie"
                    | "payment-signature"
                    | "x-payment"
                    | "sign-in-with-x"
                    | "x-sign-in-with-x"
            )
        }) || canonical_headers
            .windows(2)
            .any(|pair| pair[0].0 == pair[1].0)
        {
            return Err(rejected("PREFILLED_CREDENTIAL_OR_AMBIGUOUS_HEADER"));
        }
        let request = ImmutableRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: canonical_headers,
            body: body.map(|b| b.as_bytes().to_vec()),
        };
        let bytes = serde_json::to_vec(&request).map_err(|_| rejected("INVALID_REQUEST"))?;
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let key = (binding.caller_id.clone(), id.to_string());
        let mut operations = self
            .0
            .lock()
            .map_err(|_| rejected("REPLAY_GUARD_UNAVAILABLE"))?;
        let attempt_id = uuid::Uuid::new_v4().to_string();
        if let Some(previous) = operations.get_mut(&key) {
            if previous.digest != digest {
                return Err(rejected("OPERATION_CONFLICT"));
            }
            if !matches!(previous.state, OperationState::Retryable) {
                return Err(rejected("OPERATION_ALREADY_STARTED"));
            }
            previous.state = OperationState::Claimed(attempt_id.clone());
        } else {
            if operations.len() >= 1024 {
                return Err(rejected("REPLAY_GUARD_FULL"));
            }
            operations.insert(
                key,
                Operation {
                    digest,
                    state: OperationState::Claimed(attempt_id.clone()),
                },
            );
        }
        Ok(AuthorityAttempt {
            operations: self.clone(),
            binding,
            operation_id: id.to_string(),
            attempt_id,
            request,
        })
    }
}

impl AuthorityAttempt {
    pub(crate) fn controller(&self) -> Option<&dyn crate::controlled::ControlledExecution> {
        self.binding.controlled.as_deref()
    }
    fn mark_retryable(&self) -> Result<(), pay_core::Error> {
        let mut operations = self
            .operations
            .0
            .lock()
            .map_err(|_| rejected("REPLAY_GUARD_UNAVAILABLE"))?;
        let key = (self.binding.caller_id.clone(), self.operation_id.clone());
        match operations.get_mut(&key) {
            Some(Operation { state, .. }) if matches!(&*state, OperationState::Claimed(id) if *id == self.attempt_id) =>
            {
                *state = OperationState::Retryable;
                Ok(())
            }
            _ => Err(rejected("OPERATION_ALREADY_STARTED")),
        }
    }

    pub(crate) fn authorize(
        &self,
        challenge: &pay_core::client::x402::Challenge,
        execution_network: &str,
    ) -> Result<SpendRequest, pay_core::Error> {
        let accepted = challenge.requirements.to_accepted_value();
        // Keep this first seam narrow. Other schemes/authentication flows must not
        // fall through to a free signer, including those advertised beside exact.
        if challenge.x402_version != 2
            || challenge.siwx.is_some()
            || !accepted
                .get("network")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.starts_with("solana:"))
        {
            return Err(rejected("UNSUPPORTED_PAYMENT"));
        }
        let amount = challenge
            .requirements
            .amount
            .parse::<u64>()
            .map_err(|_| rejected("INVALID_AMOUNT"))?;
        let field = |name: &str| {
            accepted
                .get(name)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| rejected("INVALID_OFFER"))
        };
        let facts = SpendRequest {
            operation_id: self.operation_id.clone(),
            attempt_id: self.attempt_id.clone(),
            caller_id: self.binding.caller_id.clone(),
            request: self.request.clone(),
            amount_atomic: amount.to_string(),
            asset: field("asset")?,
            network: field("network")?,
            execution_network: execution_network.to_string(),
            recipient: field("payTo")?,
            accepted,
        };
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| rejected("AUTHORITY_RUNTIME_UNAVAILABLE"))?;
        let decision = runtime.block_on(async {
            tokio::time::timeout(
                self.binding.timeout,
                self.binding.authority.authorize(facts.clone()),
            )
            .await
        });
        match decision {
            Ok(Ok(AuthorityDecision::Allow)) => Ok(facts),
            Ok(Ok(AuthorityDecision::Deny)) => Err(rejected("DENIED")),
            Ok(Ok(AuthorityDecision::RejectBeforeExecution)) => {
                self.mark_retryable()?;
                Err(rejected("REJECTED_BEFORE_EXECUTION"))
            }
            Ok(Err(_)) => Err(rejected("UNAVAILABLE")),
            Err(_) => Err(rejected("TIMEOUT")),
        }
    }
}

pub(crate) fn unsupported() -> pay_core::Error {
    rejected("UNSUPPORTED_PAYMENT")
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Deny;
    impl SpendingAuthority for Deny {
        fn authorize(
            &self,
            _: SpendRequest,
        ) -> Pin<
            Box<dyn Future<Output = Result<AuthorityDecision, AuthorityUnavailable>> + Send + '_>,
        > {
            Box::pin(async { Ok(AuthorityDecision::Deny) })
        }
    }
    fn binding(caller: &str) -> AuthorityBinding {
        AuthorityBinding::new(caller, Arc::new(Deny))
    }
    // Minimal harness for curl's spawn_blocking boundary. No MCP transport,
    // wallet, signer or HTTP is involved; aborting the outer task drops the
    // response receiver while the already-entered blocking worker continues.
    struct GatedOutcome {
        first: Result<AuthorityDecision, AuthorityUnavailable>,
        requests: Mutex<Vec<SpendRequest>>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl SpendingAuthority for GatedOutcome {
        fn authorize(
            &self,
            request: SpendRequest,
        ) -> Pin<
            Box<dyn Future<Output = Result<AuthorityDecision, AuthorityUnavailable>> + Send + '_>,
        > {
            Box::pin(async move {
                let first = {
                    let mut requests = self.requests.lock().unwrap();
                    requests.push(request);
                    requests.len() == 1
                };
                if first {
                    self.entered.notify_one();
                    self.release.notified().await;
                    match self.first {
                        Ok(decision) => Ok(decision),
                        Err(_) => Err(AuthorityUnavailable),
                    }
                } else {
                    Ok(AuthorityDecision::Allow)
                }
            })
        }
    }
    fn fixture_challenge() -> pay_core::client::x402::Challenge {
        use base64::Engine;
        let offer = serde_json::json!({"x402Version":2,"accepts":[{
            "scheme":"exact","network":"solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
            "amount":"10000","asset":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "payTo":"11111111111111111111111111111111","maxTimeoutSeconds":60,
            "extra":{"decimals":6}
        }]});
        let encoded = base64::engine::general_purpose::STANDARD.encode(offer.to_string());
        pay_core::client::x402::parse(&[("Payment-Required".into(), encoded)], None).unwrap()
    }
    async fn cancelled_outer_call(first: Result<AuthorityDecision, AuthorityUnavailable>) {
        let retryable = matches!(first, Ok(AuthorityDecision::RejectBeforeExecution));
        let provider = Arc::new(GatedOutcome {
            first,
            requests: Mutex::new(Vec::new()),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let operations = Operations::default();
        let start = || {
            operations.start(
                AuthorityBinding::new("caller", provider.clone()),
                Some("cancelled"),
                "POST",
                "https://example.com",
                &[],
                Some(&RequestBody::text("original")),
            )
        };
        let attempt = start().unwrap();
        let original_id = attempt.attempt_id.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let outer = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let result = attempt.authorize(&fixture_challenge(), "mainnet");
                // Sent only after authorize has processed the verdict/transition.
                assert!(done_tx.send(result).is_ok());
            })
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), provider.entered.notified())
            .await
            .unwrap();
        outer.abort();
        assert!(outer.await.unwrap_err().is_cancelled());
        // Cancellation alone cannot release the claim before any verdict.
        assert!(
            start()
                .err()
                .unwrap()
                .to_string()
                .contains("OPERATION_ALREADY_STARTED")
        );
        provider.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .unwrap()
            .unwrap();
        let code = if retryable {
            "REJECTED_BEFORE_EXECUTION"
        } else {
            "UNAVAILABLE"
        };
        assert!(result.err().unwrap().to_string().contains(code));
        if retryable {
            let next = start().unwrap();
            assert_ne!(original_id, next.attempt_id);
            tokio::task::spawn_blocking(move || next.authorize(&fixture_challenge(), "mainnet"))
                .await
                .unwrap()
                .unwrap();
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_ne!(requests[0].attempt_id, requests[1].attempt_id);
            assert_eq!(requests[0].caller_id, requests[1].caller_id);
            assert_eq!(requests[0].operation_id, requests[1].operation_id);
            assert_eq!(
                serde_json::to_value(&requests[0].request).unwrap(),
                serde_json::to_value(&requests[1].request).unwrap()
            );
        } else {
            assert!(
                start()
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("OPERATION_ALREADY_STARTED")
            );
            assert_eq!(provider.requests.lock().unwrap().len(), 1);
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn safe_rejection_allows_retry_after_outer_cancellation_and_lost_delivery() {
        cancelled_outer_call(Ok(AuthorityDecision::RejectBeforeExecution)).await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_provider_outcome_stays_fenced_after_outer_cancellation() {
        cancelled_outer_call(Err(AuthorityUnavailable)).await;
    }
    #[test]
    fn replay_and_mutation_are_fenced_without_an_economic_ledger() {
        let operations = Operations::default();
        let request = RequestBody::text("original");
        assert!(
            operations
                .start(
                    binding("agent-a"),
                    Some("op-1"),
                    "POST",
                    "https://example.com",
                    &[],
                    Some(&request)
                )
                .is_ok()
        );
        let duplicate = operations
            .start(
                binding("agent-a"),
                Some("op-1"),
                "POST",
                "https://example.com",
                &[],
                Some(&request),
            )
            .err()
            .unwrap();
        assert!(duplicate.to_string().contains("OPERATION_ALREADY_STARTED"));
        let changed = RequestBody::text("changed");
        assert!(
            operations
                .start(
                    binding("agent-a"),
                    Some("op-1"),
                    "POST",
                    "https://example.com",
                    &[],
                    Some(&changed)
                )
                .err()
                .unwrap()
                .to_string()
                .contains("OPERATION_CONFLICT")
        );
        assert!(
            operations
                .start(
                    binding("agent-b"),
                    Some("op-1"),
                    "POST",
                    "https://example.com",
                    &[],
                    Some(&request)
                )
                .is_ok()
        );
    }
    #[test]
    fn stale_attempt_cannot_release_new_claim_even_at_capacity() {
        let operations = Operations::default();
        let start = || {
            operations.start(
                binding("a"),
                Some("op-0"),
                "GET",
                "https://example.com",
                &[],
                None,
            )
        };
        let old = start().unwrap();
        old.mark_retryable().unwrap();
        for i in 1..1024 {
            operations
                .start(
                    binding("a"),
                    Some(&format!("op-{i}")),
                    "GET",
                    "https://example.com",
                    &[],
                    None,
                )
                .unwrap();
        }
        let current = start().unwrap();
        assert_ne!(old.attempt_id, current.attempt_id);
        assert!(old.mark_retryable().is_err());
        assert!(
            start()
                .err()
                .unwrap()
                .to_string()
                .contains("OPERATION_ALREADY_STARTED")
        );
        current.mark_retryable().unwrap();
        assert!(old.mark_retryable().is_err());
        let next = start().unwrap();
        assert_ne!(current.attempt_id, next.attempt_id);
        drop(next);
        assert!(
            start()
                .err()
                .unwrap()
                .to_string()
                .contains("OPERATION_ALREADY_STARTED")
        );
    }
    #[test]
    fn simultaneous_claim_has_one_winner() {
        let operations = Arc::new(Operations::default());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let operations = operations.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    operations
                        .start(
                            binding("agent-a"),
                            Some("same"),
                            "GET",
                            "https://example.com",
                            &[],
                            None,
                        )
                        .is_ok()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        assert_eq!(
            handles
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }
    #[test]
    fn header_case_and_order_cannot_change_replay_identity() {
        let operations = Operations::default();
        let first = vec![("X-B".into(), "b".into()), ("X-A".into(), "a".into())];
        let second = vec![("x-a".into(), "a".into()), ("x-b".into(), "b".into())];
        assert!(
            operations
                .start(
                    binding("agent-a"),
                    Some("op"),
                    "GET",
                    "https://example.com",
                    &first,
                    None
                )
                .is_ok()
        );
        assert!(
            operations
                .start(
                    binding("agent-a"),
                    Some("op"),
                    "GET",
                    "https://example.com",
                    &second,
                    None
                )
                .err()
                .unwrap()
                .to_string()
                .contains("OPERATION_ALREADY_STARTED")
        );
    }
    #[test]
    fn credentials_and_invalid_configuration_fail_closed() {
        let operations = Operations::default();
        for name in [
            "Authorization",
            "COOKIE",
            "Payment-Signature",
            "X-Payment",
            "SIGN-IN-WITH-X",
        ] {
            assert!(
                operations
                    .start(
                        binding("agent"),
                        Some(name),
                        "GET",
                        "https://example.com",
                        &[(name.into(), "opaque".into())],
                        None
                    )
                    .is_err()
            );
        }
        assert!(
            operations
                .start(
                    binding(""),
                    Some("op"),
                    "GET",
                    "https://example.com",
                    &[],
                    None
                )
                .is_err()
        );
        assert!(
            operations
                .start(binding("a"), None, "GET", "https://example.com", &[], None)
                .is_err()
        );
        assert!(
            operations
                .start(
                    binding("a").with_timeout(Duration::ZERO),
                    Some("op"),
                    "GET",
                    "https://example.com",
                    &[],
                    None
                )
                .is_err()
        );
    }
    #[test]
    fn replay_guard_never_evicts_an_old_attempt_to_allow_a_new_payment() {
        let operations = Operations::default();
        for i in 0..1024 {
            assert!(
                operations
                    .start(
                        binding("a"),
                        Some(&format!("op-{i}")),
                        "GET",
                        "https://example.com",
                        &[],
                        None
                    )
                    .is_ok()
            );
        }
        assert!(
            operations
                .start(
                    binding("a"),
                    Some("overflow"),
                    "GET",
                    "https://example.com",
                    &[],
                    None
                )
                .err()
                .unwrap()
                .to_string()
                .contains("REPLAY_GUARD_FULL")
        );
        assert!(
            operations
                .start(
                    binding("a"),
                    Some("op-0"),
                    "GET",
                    "https://example.com",
                    &[],
                    None
                )
                .err()
                .unwrap()
                .to_string()
                .contains("OPERATION_ALREADY_STARTED")
        );
    }
}
