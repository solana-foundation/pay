//! Reference remote spending authority over HTTP(S).
//!
//! PR #514 (`crate::authority`) defines the `SpendingAuthority` trait and the
//! call-path guarantees; it intentionally leaves concrete policy providers to
//! hosts. This module ships the production-shaped reference provider: it
//! consults an external trust service — the Universal Trust Adapter (UTA)
//! pattern discussed in #505 — by POSTing the exact `SpendRequest` JSON and
//! mapping the verdict fail-closed.
//!
//! # Wire contract (single round trip)
//!
//! ```text
//! POST {endpoint}
//! content-type: application/json
//!
//! body: serde_json::to_vec(&SpendRequest)
//!       (headers serialize as [name, value] pairs; request body as a byte array)
//!
//! 2xx + {"decision":"allow","unavailable":false}  -> Ok(AuthorityDecision::Allow)
//! 2xx + {"decision":"deny", "unavailable":false}  -> Ok(AuthorityDecision::Deny)
//! 2xx + {"unavailable":true, ...}                 -> Err(AuthorityUnavailable)
//! 2xx + {"unavailable":<non-bool>, ...}           -> Err(AuthorityUnavailable)
//! non-2xx, malformed body, unknown verdict,
//! body larger than [`MAX_VERDICT_BYTES`],
//! transport error, timeout                        -> Err(AuthorityUnavailable)
//! ```
//!
//! Fail-closed by construction: the only path to `Allow` is a 2xx response
//! whose parsed verdict is exactly `decision:"allow"` with a false or absent
//! `unavailable` flag. The `unavailable` flag takes precedence over
//! `decision`, so a provider that failed while its fallback stance is deny
//! surfaces as unavailable — never as a silent policy denial and never as an
//! implicit allow. This mirrors the tested Node bridge semantics in
//! `universal-trust-adapter/pay-mcp-bridge/src/spend514.mjs`.
//!
//! # Security posture
//!
//! - `endpoint` is trusted host configuration, never a tool argument; it is
//!   resolved per call but supplied only by the host that constructs the
//!   `RemoteHttpAuthority`.
//! - No credentials are attached to the consultation, and redirects are not
//!   followed ([`reqwest::redirect::Policy::none`]): the frozen request and
//!   offer snapshot cannot leak to a different origin mid-flight.
//! - Verdict responses are opaque to the agent: they only ever surface as
//!   Allow/Deny/`UNAVAILABLE` through the `AuthorityAttempt` mapping. Hosts
//!   that want receipts (verdict ids, bands, Ed25519 attestations) should
//!   capture them in their own audit sink, e.g. by wrapping this authority.
//! - Deadlines belong to the `AuthorityBinding` (5s default, 60s maximum).
//!   The optional inner `request_timeout` can only narrow that window; it
//!   cannot extend it.
//!
//! # Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use pay_mcp::authority::AuthorityBinding;
//! # use pay_mcp::authority_remote::RemoteHttpAuthority;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let authority = Arc::new(RemoteHttpAuthority::new(
//!     "http://127.0.0.1:8642/v1/authority/authorize",
//! )?);
//! let binding = AuthorityBinding::new("trusted-agent-42", authority);
//! # Ok(())
//! # }
//! ```

use crate::authority::{AuthorityDecision, AuthorityUnavailable, SpendRequest, SpendingAuthority};
use serde_json::Value;
use std::{
    future::Future,
    pin::Pin,
    time::Duration,
};

/// Consults a trusted external trust service over HTTP(S) for every spend.
#[derive(Clone)]
pub struct RemoteHttpAuthority {
    endpoint: String,
    client: reqwest::Client,
    request_timeout: Option<Duration>,
}

impl RemoteHttpAuthority {
    /// `endpoint` is trusted configuration, for example
    /// `http://127.0.0.1:8642/v1/authority/authorize` for a local UTA bridge.
    pub fn new(endpoint: impl Into<String>) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            endpoint: endpoint.into(),
            client,
            request_timeout: None,
        })
    }

    /// Optional inner deadline for the consultation itself. It can only
    /// narrow the `AuthorityBinding` timeout that wraps every `authorize`
    /// call; it can never extend it.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Maps a parsed verdict to a decision. Anything that is not an
    /// unambiguous, available allow/deny fails closed.
    ///
    /// A present `unavailable` value that is not a JSON boolean is treated as
    /// a malformed verdict and fails closed: coercing it to `false` would let
    /// a broken (or hostile) authority turn an ambiguous availability flag
    /// into a spend approval.
    fn map_verdict(verdict: &Value) -> Result<AuthorityDecision, AuthorityUnavailable> {
        match verdict.get("unavailable") {
            // Absent, or an explicit `false`: proceed to the decision match.
            None | Some(Value::Bool(false)) => {}
            // Explicit `true`: the authority self-reports unavailability.
            Some(Value::Bool(true)) => return Err(AuthorityUnavailable),
            // Present but not a boolean (string, null, number, object,
            // array): malformed verdict, never coerced, fail closed.
            Some(_) => return Err(AuthorityUnavailable),
        }
        match verdict.get("decision").and_then(Value::as_str) {
            Some("allow") => Ok(AuthorityDecision::Allow),
            Some("deny") => Ok(AuthorityDecision::Deny),
            _ => Err(AuthorityUnavailable),
        }
    }
}

/// Hard cap on the buffered verdict body. A 2xx response larger than this is
/// rejected as `AuthorityUnavailable` instead of being buffered in full, so a
/// broken or hostile authority cannot grow host memory with an unbounded
/// body. Real verdicts are tens of bytes; 64 KiB leaves orders of magnitude
/// of headroom.
///
/// This is deliberately much tighter than the general response caps in
/// `pay-core` (`MAX_PUBLIC_RESPONSE_BODY_BYTES`, 64 MiB). Authority verdicts
/// are tiny, structured payloads; the tight bound is what keeps a hostile or
/// compromised authority from forcing response-sized buffering. If a
/// legitimate verdict ever exceeds the cap, the failure mode is a fail-closed
/// `AuthorityUnavailable`, never a silent truncation.
pub const MAX_VERDICT_BYTES: usize = 64 * 1024;

impl SpendingAuthority for RemoteHttpAuthority {
    fn authorize(
        &self,
        request: SpendRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AuthorityDecision, AuthorityUnavailable>> + Send + '_>>
    {
        Box::pin(async move {
            // `SpendRequest` is Serialize; a serialization failure is a host
            // bug but must still fail closed rather than panic mid-payment.
            let body = serde_json::to_vec(&request).map_err(|_| AuthorityUnavailable)?;
            let mut builder = self
                .client
                .post(&self.endpoint)
                .header("content-type", "application/json")
                .header("accept", "application/json")
                .body(body);
            if let Some(timeout) = self.request_timeout {
                builder = builder.timeout(timeout);
            }
            let mut response = builder.send().await.map_err(|_| AuthorityUnavailable)?;
            if !response.status().is_success() {
                return Err(AuthorityUnavailable);
            }
            // Stream the body under a hard cap instead of buffering the whole
            // response: `response.bytes()` would accept arbitrarily large 2xx
            // bodies from the consulted host.
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| AuthorityUnavailable)? {
                if buf.len().saturating_add(chunk.len()) > MAX_VERDICT_BYTES {
                    return Err(AuthorityUnavailable);
                }
                buf.extend_from_slice(&chunk);
            }
            let verdict: Value = serde_json::from_slice(&buf).map_err(|_| AuthorityUnavailable)?;
            Self::map_verdict(&verdict)
        })
    }
}
