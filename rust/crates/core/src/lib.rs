// Shared modules
pub mod accounts;
mod b58;
pub mod config;
pub mod error;
pub mod explorer;
pub mod instructions;
pub mod keystore;
pub mod signer;
pub mod skills;
pub mod user_agent;

// Client modules (CLI)
pub mod client;

// Flat re-exports so callers can use `pay_core::mpp`, `pay_core::runner`, etc.
pub use client::balance;
pub use client::fetch;
pub use client::mpp;
pub use client::runner;
pub use client::runner::{
    run_curl, run_curl_with_headers, run_httpie, run_httpie_with_headers, run_wget,
    run_wget_with_headers,
};
pub use client::sandbox;
pub use client::send;
pub use client::session;
pub use client::x402;

// Server modules (gateway proxy)
pub mod server;

pub use config::{Config, LogFormat};
pub use error::{Error, Result};
pub use server::{AccountingKey, AccountingStore, InMemoryStore, current_period};
pub use user_agent::ClientApp;

#[cfg(feature = "server")]
pub use pay_kit::mpp as solana_mpp;
#[cfg(feature = "server")]
use pay_kit::mpp::server::Mpp;
#[cfg(feature = "server")]
use pay_types::metering::ApiSpec;
#[cfg(feature = "server")]
use std::sync::Arc;

/// Trait that the application state must implement for the payment middleware.
#[cfg(feature = "server")]
pub trait PaymentState: Clone + Send + Sync + 'static {
    fn apis(&self) -> &[ApiSpec];
    fn mpp(&self) -> Option<&Mpp>;
    fn mpps(&self) -> Vec<&Mpp> {
        self.mpp().into_iter().collect()
    }
    fn browser_rpc_url(&self) -> Option<&str> {
        None
    }
    fn session_mpp(&self) -> Option<&server::session::SessionMpp> {
        None
    }
    fn session_mpp_handle(&self) -> Option<Arc<server::session::SessionMpp>> {
        None
    }
    fn session_mpps(&self) -> Vec<&server::session::SessionMpp> {
        self.session_mpp().into_iter().collect()
    }
    fn session_mpp_handles(&self) -> Vec<Arc<server::session::SessionMpp>> {
        self.session_mpp_handle().into_iter().collect()
    }
    fn fee_payer_wallet(&self) -> Option<&server::telemetry::FeePayerWallet> {
        None
    }
    /// Operator's fee-payer signer, when configured. The subscription
    /// middleware needs it at verify time to co-sign the activation
    /// transaction; charge / session paths construct their own MPP
    /// instances at startup and don't ask for it through this trait.
    fn fee_payer_signer(&self) -> Option<Arc<dyn pay_kit::mpp::solana_keychain::SolanaSigner>> {
        None
    }

    /// Store used to bind a confirmed subscription activation to its reusable
    /// bearer proof. Hosts must return the same store across requests; otherwise
    /// a proof accepted during activation cannot be recognized on later access.
    fn subscription_store(&self) -> Option<Arc<dyn pay_kit::mpp::store::Store>> {
        None
    }

    /// x402 `exact` handler, when the server accepts x402 payments.
    fn x402(&self) -> Option<&pay_kit::x402::server::X402> {
        None
    }
    /// x402 `upto` (usage-based) handler, when configured with an operator signer.
    fn x402_upto(&self) -> Option<&pay_kit::x402::server::X402Upto> {
        None
    }
    /// x402 `batch-settlement` handler, when configured with an operator signer.
    fn x402_batch(&self) -> Option<&pay_kit::x402::server::X402BatchSettlement> {
        None
    }

    /// Record a completed proxied HTTP exchange for the Payment Debugger.
    ///
    /// Default no-op. The gate calls this once per proxied request; hosts with
    /// the debugger enabled ingest it into the PDB correlation engine. This is
    /// how proxied traffic reaches PDB now that the data plane is Pingora (which
    /// bypasses the old axum `logging_middleware`).
    fn record_exchange(&self, _exchange: HttpExchange) {}

    /// Whether the host consumes completed HTTP exchanges.
    ///
    /// Defaults to `true` to preserve logging for external implementations
    /// that override [`Self::record_exchange`]. Hosts that can determine at
    /// runtime that logging is disabled should return `false`, allowing the
    /// proxy to skip request/response header materialization entirely.
    fn records_http_exchanges(&self) -> bool {
        true
    }

    /// Called at request time, before the upstream responds. A host that
    /// tracks in-flight requests (`pay gate inference`) returns a log id;
    /// the gate echoes it in [`HttpExchange::log_id`] and in
    /// [`PaymentState::record_exchange_update`] calls. Returning `None`
    /// (the default) also disables the gate's response stream observer for
    /// the request — zero overhead for hosts that don't opt in.
    fn record_request_start(&self, _start: &RequestStart) -> Option<u64> {
        None
    }

    /// Live telemetry for an in-flight request (running token counts, TTFT),
    /// emitted by the gate's response stream observer at most ~1/s. Default
    /// no-op.
    fn record_exchange_update(&self, _log_id: u64, _usage: &InferenceUsage) {}
}

/// A completed HTTP exchange handed to [`PaymentState::record_exchange`].
#[derive(Debug, Clone)]
pub struct HttpExchange {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ms: u64,
    pub req_headers: Vec<(String, String)>,
    pub res_headers: Vec<(String, String)>,
    pub client_ip: String,
    /// Id returned by [`PaymentState::record_request_start`], echoed back so
    /// the host can close the in-flight record it opened.
    pub log_id: Option<u64>,
    /// Final inference telemetry from the gate's stream observer, when the
    /// host opted in via `record_request_start`.
    pub usage: Option<InferenceUsage>,
    /// Pricing/settlement outcome for this exchange, when the endpoint is
    /// metered. `None` for endpoints the gate never prices at all (control
    /// plane, unmetered subscription auth) — distinct from
    /// [`ChargeStatus::NotCharged`], which means a metered endpoint's request
    /// specifically wasn't charged (free tier, short-circuited before
    /// payment).
    pub charge: Option<ChargeOutcome>,
}

/// Outcome of a proxied request's pricing, independent of on-chain
/// settlement timing — a request can be reportable (unit, quantity, USD
/// amount known) before its payment is confirmed on-chain. Covers every
/// [`pay_types::metering::Scheme`]: charge-style schemes (`mpp/charge`,
/// `x402/exact`, `x402/batch`) know this synchronously before forwarding;
/// usage-metered schemes (`x402/upto`, `mpp/session`) only know it once the
/// response has been served and settlement computed.
#[derive(Debug, Clone)]
pub struct ChargeOutcome {
    /// The endpoint's declared subdomain (e.g. `"vision"`, `"bigquery"`) —
    /// how a deployment maps this request to a specific proxied API. Always
    /// present whenever a `ChargeOutcome` exists at all.
    pub subdomain: String,
    pub scheme: pay_types::metering::Scheme,
    pub status: ChargeStatus,
    /// `None` iff `status` is [`ChargeStatus::NotCharged`].
    pub currency: Option<String>,
    /// `None` iff `status` is [`ChargeStatus::NotCharged`].
    pub amount_usd: Option<f64>,
    /// The billing dimension that priced this request (e.g. `"tokens"`,
    /// `"requests"`, `"bytes"`) — the serialized form of
    /// [`pay_types::metering::BillingUnit`], mirroring
    /// [`pay_types::metering::ResolvedDimension::unit`]. `None` when the
    /// endpoint's `paywall.yml` doesn't resolve one for this request.
    pub unit: Option<String>,
    /// Quantity billed in `unit`, when the gate computed an exact count
    /// (e.g. a usage-metered token/byte count). `None` for flat per-call
    /// pricing, where quantity is implicitly 1 request.
    pub quantity: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeStatus {
    /// Served and settled for a non-zero amount (on-chain confirmation may
    /// still be pending for deferred schemes).
    Charged,
    /// Served, but settled to zero — a refund (failed delivery, missing-usage
    /// policy, or a `!served_ok` post-response settlement).
    Refunded,
    /// Not charged at all: free tier, or short-circuited before any payment
    /// was verified.
    NotCharged,
    /// The resource was served, but the settlement attempt itself errored
    /// (e.g. a deferred on-chain settle broadcast failed) — distinct from a
    /// deliberate `Refunded`. The amount, if any, is unknown.
    Failed,
}

/// A metered exchange flattened for wire transport to an external reporting
/// pipeline (JSON over a Redis Stream, today) — the same shape for every
/// [`pay_types::metering::Scheme`], so a single downstream consumer (e.g. a
/// partner's usage/billing feed) never needs to know which payment protocol
/// served a given request.
///
/// Hosts construct this from a completed [`HttpExchange`] inside
/// [`PaymentState::record_exchange`] and hand it to a bounded, non-blocking
/// sink — never anything that could stall the request/response cycle that
/// produced it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BillingEvent {
    pub method: String,
    pub path: String,
    /// `Host` header from the original request, when present — the
    /// strongest per-deployment signal of which proxied fleet/domain
    /// served this request (e.g. `vision.google-sandbox.example.com`
    /// distinguishes provider and environment, not just API name).
    pub host: Option<String>,
    /// The endpoint's declared subdomain (e.g. `"vision"`, `"bigquery"`) —
    /// identifies which proxied API served this request, independent of
    /// deployment domain naming.
    pub subdomain: String,
    pub status: u16,
    pub ms: u64,
    pub scheme: pay_types::metering::Scheme,
    pub charge_status: ChargeStatus,
    pub currency: Option<String>,
    pub amount_usd: Option<f64>,
    pub unit: Option<String>,
    pub quantity: Option<u64>,
}

impl BillingEvent {
    /// `None` when the exchange has no charge to report at all — a request
    /// outside the per-call metering model entirely (e.g. subscription
    /// auth), not merely one that wasn't charged (that's
    /// `charge_status: NotCharged`, still reported).
    pub fn from_exchange(exchange: &HttpExchange) -> Option<Self> {
        let charge = exchange.charge.as_ref()?;
        let host = exchange
            .req_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.clone());
        Some(Self {
            method: exchange.method.clone(),
            path: exchange.path.clone(),
            host,
            subdomain: charge.subdomain.clone(),
            status: exchange.status,
            ms: exchange.ms,
            scheme: charge.scheme,
            charge_status: charge.status,
            currency: charge.currency.clone(),
            amount_usd: charge.amount_usd,
            unit: charge.unit.clone(),
            quantity: charge.quantity,
        })
    }
}

/// Request-side facts handed to [`PaymentState::record_request_start`].
#[derive(Debug, Clone)]
pub struct RequestStart {
    pub method: String,
    pub path: String,
    /// `Host` header — how the host maps the request to an API/provider.
    pub host: Option<String>,
    pub client_ip: String,
    /// The request carries a payment credential — MPP `Authorization:
    /// Payment`, x402 `PAYMENT-SIGNATURE`, or x402 v1 `X-PAYMENT`. Lets the
    /// host merge the challenge and its retry into one tracked exchange.
    pub payment: bool,
}

/// Inference telemetry accumulated by the gate's response stream observer
/// (OpenAI-compatible SSE, Ollama-native NDJSON, or plain JSON bodies).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InferenceUsage {
    /// Model name reported by the upstream response.
    pub model: Option<String>,
    /// Response was a stream (`text/event-stream` / `application/x-ndjson`).
    pub streamed: bool,
    /// Time to first response body byte, from request receipt.
    pub ttft_ms: Option<u64>,
    pub tokens_prompt: Option<u64>,
    /// Authoritative when the upstream reported usage; otherwise approximated
    /// live from stream events and overwritten if a final count arrives.
    pub tokens_completion: Option<u64>,
    pub tokens_per_sec: Option<f64>,
}

#[cfg(test)]
mod billing_event_tests {
    use super::*;

    fn exchange(charge: Option<ChargeOutcome>) -> HttpExchange {
        HttpExchange {
            method: "POST".to_string(),
            path: "v1/simple/echo".to_string(),
            status: 200,
            ms: 42,
            req_headers: Vec::new(),
            res_headers: Vec::new(),
            client_ip: "127.0.0.1".to_string(),
            log_id: None,
            usage: None,
            charge,
        }
    }

    #[test]
    fn no_charge_at_all_produces_no_billing_event() {
        assert!(BillingEvent::from_exchange(&exchange(None)).is_none());
    }

    #[test]
    fn host_header_is_extracted_case_insensitively() {
        let charge = ChargeOutcome {
            subdomain: "vision".to_string(),
            scheme: pay_types::metering::Scheme::MppCharge,
            status: ChargeStatus::Charged,
            currency: Some("SOL".to_string()),
            amount_usd: Some(0.01),
            unit: None,
            quantity: None,
        };
        let mut exchange = exchange(Some(charge));
        // Real proxied requests carry a title-case `Host` header, but
        // nothing in the HTTP spec requires that exact casing — the lookup
        // must not depend on it.
        exchange.req_headers = vec![(
            "HOST".to_string(),
            "vision.google-sandbox.example.com".to_string(),
        )];

        let event = BillingEvent::from_exchange(&exchange).unwrap();
        assert_eq!(
            event.host.as_deref(),
            Some("vision.google-sandbox.example.com")
        );
        assert_eq!(event.subdomain, "vision");
    }

    #[test]
    fn missing_host_header_reports_none_not_an_error() {
        let charge = ChargeOutcome {
            subdomain: "vision".to_string(),
            scheme: pay_types::metering::Scheme::MppCharge,
            status: ChargeStatus::Charged,
            currency: Some("SOL".to_string()),
            amount_usd: Some(0.01),
            unit: None,
            quantity: None,
        };
        let event = BillingEvent::from_exchange(&exchange(Some(charge))).unwrap();
        assert_eq!(event.host, None);
    }

    #[test]
    fn a_charged_exchange_maps_every_field() {
        let charge = ChargeOutcome {
            subdomain: "vision".to_string(),
            scheme: pay_types::metering::Scheme::MppCharge,
            status: ChargeStatus::Charged,
            currency: Some("SOL".to_string()),
            amount_usd: Some(0.01),
            unit: Some("requests".to_string()),
            quantity: None,
        };
        let event = BillingEvent::from_exchange(&exchange(Some(charge)))
            .expect("a charge always produces an event");

        assert_eq!(event.method, "POST");
        assert_eq!(event.path, "v1/simple/echo");
        assert_eq!(event.subdomain, "vision");
        assert_eq!(event.status, 200);
        assert_eq!(event.ms, 42);
        assert_eq!(event.scheme, pay_types::metering::Scheme::MppCharge);
        assert_eq!(event.charge_status, ChargeStatus::Charged);
        assert_eq!(event.currency.as_deref(), Some("SOL"));
        assert_eq!(event.amount_usd, Some(0.01));
        assert_eq!(event.unit.as_deref(), Some("requests"));
    }

    /// Locks the wire contract `report-billing-events` (a separate crate,
    /// deserializing untyped JSON) depends on. A serde rename on either
    /// enum would silently break that consumer without a compile error
    /// anywhere in this crate — this test is what would catch it.
    #[test]
    fn a_not_charged_exchange_serializes_to_the_consumer_expected_wire_shape() {
        let charge = ChargeOutcome {
            subdomain: "vision".to_string(),
            scheme: pay_types::metering::Scheme::MppSession,
            status: ChargeStatus::NotCharged,
            currency: None,
            amount_usd: None,
            unit: None,
            quantity: None,
        };
        let event = BillingEvent::from_exchange(&exchange(Some(charge))).unwrap();
        let json = serde_json::to_value(&event).unwrap();

        assert_eq!(json["scheme"], "mpp-session");
        assert_eq!(json["charge_status"], "not_charged");
        assert_eq!(json["currency"], serde_json::Value::Null);
        assert_eq!(json["amount_usd"], serde_json::Value::Null);
    }
}
