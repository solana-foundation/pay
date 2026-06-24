//! Framework-agnostic payment gate.
//!
//! [`PaymentGate::evaluate`] is the single source of truth for "what should
//! happen to this request" — discovery passthrough, the HTTP 402 challenge, and
//! credential verification across the charge / session / subscription paths. It
//! reads only request **metadata** (never the body) and returns a
//! [`GateDecision`] describing the outcome in framework-neutral terms.
//!
//! Thin adapters map a framework's request/response onto this core:
//! - the axum `payment_middleware` (this crate), and
//! - `Http402Gate` (the Pingora `ProxyHttp` gateway, `pay-proxy` crate).
//!
//! Keeping the decision here means the gating logic lives once and is unit
//! testable without any HTTP framework.

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, StatusCode, header};
use pay_kit::mpp::server::{ChargeOptions, VerificationError};
use pay_kit::mpp::{
    PAYMENT_RECEIPT_HEADER, ReceiptKind, format_receipt, format_www_authenticate,
    format_www_authenticate_many, parse_authorization,
};
use pay_kit::x402::PAYMENT_RESPONSE_HEADER;
use pay_kit::x402::server::{ExactOptions, VerifiedExactPayment, X402, X402BatchSettlement};
use pay_types::metering::Scheme;
use serde_json::json;

use crate::PaymentState;
use crate::server::metering;
use crate::server::session::{SessionMpp, SessionOutcome};
use crate::server::telemetry;

/// `payment-receipt-url` — shareable `pay.sh/receipt/<sig>` link.
const PAYMENT_RECEIPT_URL: HeaderName = HeaderName::from_static("payment-receipt-url");

/// CSP for the rendered HTML 402 payment page.
const PAYMENT_PAGE_CSP: &str = "\
    default-src 'self'; \
    script-src 'unsafe-inline'; \
    style-src 'unsafe-inline'; \
    img-src 'self' data: blob: https:; \
    connect-src 'self' http://localhost:* http://127.0.0.1:* https:; \
    worker-src 'self'";

/// Everything the gate needs from a request. No body — the decision is made
/// from metadata alone, so the body can stream straight to the upstream.
pub struct GateRequest<'a> {
    pub method: &'a Method,
    /// Path with the leading `/` trimmed (e.g. `v1/chat`).
    pub path: &'a str,
    pub host: Option<&'a str>,
    pub accept: Option<&'a str>,
    pub authorization: Option<&'a str>,
    pub content_length: Option<u64>,
    pub query: Option<&'a str>,
    /// x402 payment header value (`PAYMENT-SIGNATURE` or `X-PAYMENT`), if present.
    pub x402_payment: Option<&'a str>,
}

/// A complete response the adapter should send as-is. `headers` is a `Vec` so
/// duplicate `WWW-Authenticate` lines (RFC 7235, one per currency) are preserved.
pub struct GateResponse {
    pub status: StatusCode,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Bytes,
}

impl GateResponse {
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }
    pub fn header(mut self, name: HeaderName, value: impl Into<String>) -> Self {
        if let Ok(v) = HeaderValue::from_str(&value.into()) {
            self.headers.push((name, v));
        }
        self
    }
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }
    pub fn json(status: StatusCode, body: impl Into<Bytes>) -> Self {
        Self::new(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
    }
}

/// Annotation applied to the upstream response after a verified payment: the
/// `payment-receipt-url` / `payment-receipt-header` lines and the settlement
/// signature (recorded on the trace span).
pub struct ReceiptAnnotation {
    /// Response headers to set on the forwarded response — protocol-specific:
    /// MPP sets `payment-receipt-url` + `payment-receipt-header`, x402 sets
    /// `PAYMENT-RESPONSE`.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Settlement reference / signature, recorded as `tx_sig` on the span.
    pub reference: Option<String>,
}

/// Session-stream metering context for a forwarded session request. The
/// adapter attaches it so the response-stream metering layer can debit the
/// channel as bytes flow back.
pub struct SessionForward {
    pub handle: Arc<SessionMpp>,
    pub channel_id: String,
    pub committed_base_units: u64,
}

/// The outcome of gating a request.
pub enum GateDecision {
    /// Send this response now and stop (402 challenge, service-worker JS, 404,
    /// a 200 receipt JSON, …).
    Respond(GateResponse),
    /// Payment verified — forward to the endpoint's configured upstream. When a
    /// session credential opened/advanced a channel, `session` carries the
    /// stream-metering context the adapter attaches to the upstream request;
    /// `receipt` is applied to the response.
    Forward {
        session: Option<SessionForward>,
        receipt: Option<ReceiptAnnotation>,
    },
    /// Not gated (discovery / free / unknown) — let normal routing handle it
    /// (forward to the default upstream, or serve a control-plane route).
    Passthrough,
}

/// The framework-agnostic payment gate, parameterized over the host's
/// [`PaymentState`] (MPP / session / subscription backends + API specs).
pub struct PaymentGate<S: PaymentState> {
    state: S,
}

impl<S: PaymentState> PaymentGate<S> {
    pub fn new(state: S) -> Self {
        Self { state }
    }

    /// Decide what to do with `req`. See the module docs for the full tree.
    pub async fn evaluate(&self, req: &GateRequest<'_>) -> GateDecision {
        use pay_kit::mpp::server::html as mpp_html;

        let path = req.path;

        // Control-plane + discovery surfaces stay unauthenticated.
        if path.starts_with("__402/") || path == "openapi.json" || path.starts_with(".well-known/")
        {
            return GateDecision::Passthrough;
        }

        let subdomain = req.host.unwrap_or("").split('.').next().unwrap_or("");
        let accepts_html = req.accept.is_some_and(mpp_html::accepts_html);

        let apis = self.state.apis();
        let api = match apis.iter().find(|a| a.subdomain == subdomain) {
            Some(api) => api,
            // Single-API mode: one configured API serves any subdomain.
            None if apis.len() == 1 => &apis[0],
            None => return GateDecision::Passthrough,
        };

        // Service worker for the HTML payment-link UI — before metering lookup
        // so it works for any path/method.
        if req
            .query
            .unwrap_or("")
            .contains(mpp_html::SERVICE_WORKER_PARAM)
        {
            return GateDecision::Respond(
                GateResponse::new(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/javascript")
                    .header(HeaderName::from_static("service-worker-allowed"), "/")
                    .body(mpp_html::service_worker_js()),
            );
        }

        // HEAD is gated like GET.
        let match_method = if req.method == Method::HEAD {
            "GET"
        } else {
            req.method.as_str()
        };
        let exact_match = metering::find_endpoint(api, match_method, path);
        let endpoint = exact_match.or_else(|| {
            // Browsers often GET a POST-only paid endpoint via a payment link;
            // fall back to path-only resolution so we can render the 402 page.
            if accepts_html {
                metering::find_endpoint_by_path(api, path)
            } else {
                None
            }
        });
        let metering_config = endpoint.and_then(|ep| ep.metering.as_ref());
        let subscription_config = endpoint.and_then(|ep| ep.subscription.as_ref());

        if metering_config.is_none() && subscription_config.is_none() {
            // Respond-routing with a known path but wrong method → 404 (no
            // upstream to fall through to).
            if api.routing.is_respond()
                && exact_match.is_none()
                && metering::find_endpoint_by_path(api, path).is_some()
            {
                return GateDecision::Respond(GateResponse::json(
                    StatusCode::NOT_FOUND,
                    Bytes::from_static(br#"{"error":"not_found","message":"method not allowed"}"#),
                ));
            }
            return GateDecision::Passthrough;
        }

        // ── Gated endpoint ──────────────────────────────────────────────────
        // (Not wired into any adapter yet — the axum middleware still owns these
        // paths — so unimplemented arms below are never reached in production.)
        if let Some(spec) = subscription_config {
            let description = endpoint.and_then(|e| e.description.as_deref());
            return self
                .evaluate_subscription(api, spec, description, req, subdomain, path)
                .await;
        }
        let meter = metering_config.expect("gated endpoint has metering");
        let accepted = meter.accepted_schemes();

        let session_handle = self.state.session_mpp_handle();
        let session_mpp = session_handle
            .as_deref()
            .or_else(|| self.state.session_mpp());

        // MPP credential present → dispatch by intent (only if accepted). A
        // present-but-unparseable credential is a client error (400).
        if let Some(auth) = req.authorization {
            match parse_authorization(auth) {
                Ok(cred) => {
                    let intent = cred.challenge.intent.as_str();
                    if intent == "session"
                        && accepted.contains(&Scheme::MppSession)
                        && let Some(sm) = session_mpp
                    {
                        return session_authorized(
                            sm,
                            session_handle.clone(),
                            auth,
                            subdomain,
                            path,
                        )
                        .await;
                    }
                    if intent == "charge" && accepted.contains(&Scheme::MppCharge) {
                        let description = endpoint.and_then(|e| e.description.as_deref());
                        return self
                            .charge_verify(api, meter, description, auth, subdomain, path, req)
                            .await;
                    }
                    // Parseable but not an accepted scheme → fall through to re-challenge.
                }
                Err(e) => {
                    return GateDecision::Respond(GateResponse::json(
                        StatusCode::BAD_REQUEST,
                        serde_json::to_vec(&json!({
                            "error": "malformed_credential", "message": e.to_string()
                        }))
                        .unwrap_or_default(),
                    ));
                }
            }
        }

        // x402 credential (PAYMENT-SIGNATURE / X-PAYMENT) → dispatch by scheme.
        if let Some(pay_header) = req.x402_payment {
            if accepted.contains(&Scheme::X402Exact)
                && let Some(x402) = self.state.x402()
            {
                return self
                    .x402_exact_verify(x402, meter, req, path, pay_header, subdomain)
                    .await;
            }
            if accepted.contains(&Scheme::X402BatchSettlement)
                && let Some(batch) = self.state.x402_batch()
            {
                return self
                    .x402_batch_verify(batch, meter, req, path, pay_header, subdomain)
                    .await;
            }
            if accepted.contains(&Scheme::X402Upto) {
                // `upto` is verify-open → meter-usage → settle-after: a
                // post-handler lifecycle that lands with the adapter phase
                // (alongside session-stream metering).
                unimplemented!(
                    "PaymentGate::evaluate: x402 upto (adapter-phase settlement lifecycle)"
                );
            }
        }

        // No (matching) credential → advertise every accepted + available scheme.
        let description = endpoint.and_then(|e| e.description.as_deref());
        self.build_challenge(
            api,
            meter,
            &accepted,
            session_mpp,
            req,
            subdomain,
            path,
            description,
            accepts_html,
        )
    }

    /// Assemble a single 402 advertising one challenge per accepted scheme that
    /// the server has a backend for (session `WWW-Authenticate` + per-MPP charge
    /// `WWW-Authenticate`; x402 `PAYMENT-REQUIRED` to follow). Fails closed (500)
    /// if a metered endpoint has no usable backend for any accepted scheme.
    #[allow(clippy::too_many_arguments)]
    fn build_challenge(
        &self,
        api: &pay_types::metering::ApiSpec,
        meter: &pay_types::metering::Metering,
        accepted: &[Scheme],
        session_mpp: Option<&SessionMpp>,
        req: &GateRequest<'_>,
        subdomain: &str,
        path: &str,
        description: Option<&str>,
        accepts_html: bool,
    ) -> GateDecision {
        // When set, render the browser HTML 402 page from this charge challenge.
        let mut html_challenge: Option<(pay_kit::mpp::PaymentChallenge, String, String)> = None;
        let gen_failed = || {
            GateDecision::Respond(GateResponse::json(
                StatusCode::INTERNAL_SERVER_ERROR,
                Bytes::from_static(br#"{"error":"challenge_generation_failed"}"#),
            ))
        };
        let props = metering::RequestProperties {
            body_size: req.content_length,
            ..Default::default()
        };
        let variant = variant_hint_from_path(path);
        let price = metering::resolve_price(meter, &props, variant.as_deref(), None);

        let mut challenge_headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
        let mut advertised: Vec<&str> = Vec::new();

        if accepted.contains(&Scheme::MppSession)
            && let Some(sm) = session_mpp
        {
            match sm.challenge_header(u64::MAX) {
                Ok(h) => {
                    if let Ok(v) = HeaderValue::from_str(&h) {
                        challenge_headers.push((header::WWW_AUTHENTICATE, v));
                        advertised.push("session");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "session challenge generation failed");
                    return gen_failed();
                }
            }
        }

        if accepted.contains(&Scheme::MppCharge) {
            let mpps = self.state.mpps();
            if !mpps.is_empty() {
                let amount = crate::server::payment::charge_amount_from_price(price.as_ref());
                let uri = reconstruct_uri(path, req.query);
                let mut challenges = Vec::with_capacity(mpps.len());
                for mpp in &mpps {
                    match mpp.charge_with_options(
                        &amount,
                        ChargeOptions {
                            description,
                            splits: crate::server::payment::resolve_charge_splits(
                                mpp, meter, api, &uri, &amount,
                            ),
                            ..Default::default()
                        },
                    ) {
                        Ok(c) => challenges.push(c),
                        Err(e) => {
                            telemetry::record_challenge_error(
                                "mpp",
                                mpp.currency(),
                                &e.to_string(),
                            );
                            return gen_failed();
                        }
                    }
                }
                match format_www_authenticate_many(&challenges) {
                    Ok(v) => {
                        for w in v {
                            if let Ok(hv) = HeaderValue::from_str(&w) {
                                challenge_headers.push((header::WWW_AUTHENTICATE, hv));
                            }
                        }
                        advertised.push("mpp");
                    }
                    Err(_) => return gen_failed(),
                }
                // Browser payment-link UI: render the first charge challenge as HTML.
                if accepts_html
                    && let (Some(ch), Some(mpp)) = (challenges.into_iter().next(), mpps.first())
                {
                    let rpc = self
                        .state
                        .browser_rpc_url()
                        .map(str::to_string)
                        .unwrap_or_else(|| mpp.rpc_url().to_string());
                    html_challenge = Some((ch, rpc, mpp.network().to_string()));
                }
            }
        }

        if accepted.contains(&Scheme::X402Exact)
            && let Some(x402) = self.state.x402()
        {
            let amount = crate::server::payment::charge_amount_from_price(price.as_ref());
            match x402.payment_required_header(&amount, ExactOptions::default()) {
                Ok((name, value)) => {
                    if let (Ok(n), Ok(v)) = (
                        HeaderName::from_bytes(name.as_bytes()),
                        HeaderValue::from_str(&value),
                    ) {
                        challenge_headers.push((n, v));
                        advertised.push("x402");
                    }
                }
                // Drop only the x402 challenge on error — MPP clients are unaffected.
                Err(e) => tracing::warn!(error = %e, "x402 challenge generation failed"),
            }
        }

        if accepted.contains(&Scheme::X402BatchSettlement)
            && let Some(batch) = self.state.x402_batch()
        {
            let amount = crate::server::payment::charge_amount_from_price(price.as_ref());
            match batch.payment_required_header(&amount) {
                Ok((name, value)) => {
                    if let (Ok(n), Ok(v)) = (
                        HeaderName::from_bytes(name.as_bytes()),
                        HeaderValue::from_str(&value),
                    ) {
                        challenge_headers.push((n, v));
                        advertised.push("x402-batch");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "x402 batch challenge generation failed"),
            }
        }

        if challenge_headers.is_empty() {
            // Metered, but no configured backend for any accepted scheme — fail
            // closed rather than serve the resource for free.
            return GateDecision::Respond(GateResponse::json(
                StatusCode::INTERNAL_SERVER_ERROR,
                Bytes::from_static(
                    br#"{"error":"payment_backend_unconfigured","message":"No payment backend for the accepted schemes."}"#,
                ),
            ));
        }

        let amount_usd = price
            .as_ref()
            .and_then(|p| p.dimensions.first())
            .map(|d| d.price_usd / d.scale.max(1) as f64);
        telemetry::record_402_challenge_sent(
            "mpp",
            subdomain,
            path,
            req.method.as_str(),
            amount_usd,
            &advertised.join(","),
            challenge_headers.len(),
        );

        // Browser flow: render the HTML payment page instead of JSON.
        if let Some((challenge, rpc_url, network)) = html_challenge {
            let page =
                pay_kit::mpp::server::html::challenge_to_html(&challenge, &rpc_url, &network);
            let mut resp = GateResponse::new(StatusCode::PAYMENT_REQUIRED)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CONTENT_SECURITY_POLICY, PAYMENT_PAGE_CSP)
                .body(Bytes::from(page));
            resp.headers.extend(challenge_headers);
            return GateDecision::Respond(resp);
        }

        let body = json!({
            "error": "payment_required",
            "message": "This endpoint requires payment.",
            "endpoint": { "method": req.method.as_str(), "path": path },
            "pricing": price,
            "payment": { "schemes": advertised },
        });
        let mut resp = GateResponse::json(
            StatusCode::PAYMENT_REQUIRED,
            serde_json::to_vec(&body).unwrap_or_default(),
        );
        resp.headers.extend(challenge_headers);
        GateDecision::Respond(resp)
    }

    /// Verify an x402 `exact` payment. On success, forward with a `PAYMENT-RESPONSE`
    /// receipt; on failure or a referenceless payment, re-challenge with 402.
    async fn x402_exact_verify(
        &self,
        x402: &X402,
        meter: &pay_types::metering::Metering,
        req: &GateRequest<'_>,
        path: &str,
        pay_header: &str,
        subdomain: &str,
    ) -> GateDecision {
        let props = metering::RequestProperties {
            body_size: req.content_length,
            ..Default::default()
        };
        let variant = variant_hint_from_path(path);
        let amount = crate::server::payment::charge_amount_from_price(
            metering::resolve_price(meter, &props, variant.as_deref(), None).as_ref(),
        );
        let reject = |msg: String| {
            telemetry::record_settlement_error("x402", subdomain, path, &msg, true);
            GateDecision::Respond(GateResponse::json(
                StatusCode::PAYMENT_REQUIRED,
                serde_json::to_vec(&json!({"error":"verification_failed","message":msg}))
                    .unwrap_or_default(),
            ))
        };
        match x402
            .process_payment(pay_header, &amount, ExactOptions::default())
            .await
        {
            Ok(verified) => match x402_reference(&verified) {
                Some(reference) => {
                    telemetry::record_payment_collected("x402", subdomain, path, None, &reference);
                    let mut headers = Vec::new();
                    if let Ok(n) = HeaderName::from_bytes(PAYMENT_RESPONSE_HEADER.as_bytes())
                        && let Ok(v) = HeaderValue::from_str(&reference)
                    {
                        headers.push((n, v));
                    }
                    GateDecision::Forward {
                        session: None,
                        receipt: Some(ReceiptAnnotation {
                            headers,
                            reference: Some(reference),
                        }),
                    }
                }
                None => reject("x402 payment carried no settlement reference".to_string()),
            },
            Err(e) => reject(e.to_string()),
        }
    }

    /// Verify an x402 `batch-settlement` payment. On `serve`, forward with the
    /// settlement header; on a cooperative refund, acknowledge (200) without
    /// serving; on failure, re-challenge. On-chain settlement is batched out of
    /// band by the operator.
    async fn x402_batch_verify(
        &self,
        batch: &X402BatchSettlement,
        meter: &pay_types::metering::Metering,
        req: &GateRequest<'_>,
        path: &str,
        pay_header: &str,
        subdomain: &str,
    ) -> GateDecision {
        let props = metering::RequestProperties {
            body_size: req.content_length,
            ..Default::default()
        };
        let variant = variant_hint_from_path(path);
        let amount = crate::server::payment::charge_amount_from_price(
            metering::resolve_price(meter, &props, variant.as_deref(), None).as_ref(),
        );
        match batch.verify_payment(pay_header, &amount).await {
            Ok(outcome) => {
                let mut headers = Vec::new();
                if let Ok((name, value)) = batch.settlement_header(&outcome.response)
                    && let (Ok(n), Ok(v)) = (
                        HeaderName::from_bytes(name.as_bytes()),
                        HeaderValue::from_str(&value),
                    )
                {
                    headers.push((n, v));
                }
                let reference = outcome
                    .response
                    .channel_state
                    .as_ref()
                    .map(|c| c.channel_id.clone());
                if outcome.serve {
                    if let Some(r) = &reference {
                        telemetry::record_payment_collected("x402", subdomain, path, None, r);
                    }
                    GateDecision::Forward {
                        session: None,
                        receipt: Some(ReceiptAnnotation { headers, reference }),
                    }
                } else {
                    // Cooperative refund / channel close — acknowledge, don't serve.
                    let mut resp = GateResponse::json(
                        StatusCode::OK,
                        Bytes::from_static(br#"{"status":"channel_closed"}"#),
                    );
                    resp.headers.extend(headers);
                    GateDecision::Respond(resp)
                }
            }
            Err(e) => {
                telemetry::record_settlement_error("x402", subdomain, path, &e.to_string(), true);
                GateDecision::Respond(GateResponse::json(
                    StatusCode::PAYMENT_REQUIRED,
                    serde_json::to_vec(
                        &json!({"error":"verification_failed","message":e.to_string()}),
                    )
                    .unwrap_or_default(),
                ))
            }
        }
    }

    /// Subscription endpoint: no auth → 402 (subscription + authenticate
    /// challenges); `authenticate` intent → stateless verify → forward / 402;
    /// `subscription` intent → activation → forward (+ receipt + "next time"
    /// authenticate challenge) / 402.
    async fn evaluate_subscription(
        &self,
        api: &pay_types::metering::ApiSpec,
        spec: &pay_types::metering::SubscriptionEndpoint,
        description: Option<&str>,
        req: &GateRequest<'_>,
        subdomain: &str,
        path: &str,
    ) -> GateDecision {
        use crate::server::{authenticate, subscription as sub};

        let mpps = self.state.mpps();
        let operator = api.operator.as_ref();
        let Some(puller) = operator
            .and_then(|o| o.recipient.clone())
            .or_else(|| mpps.first().map(|m| m.recipient().to_string()))
        else {
            return GateDecision::Respond(GateResponse::json(
                StatusCode::INTERNAL_SERVER_ERROR,
                Bytes::from_static(
                    br#"{"error":"subscription_misconfigured","message":"missing operator.recipient"}"#,
                ),
            ));
        };
        let recipient = spec.recipient.clone().unwrap_or_else(|| puller.clone());
        let network = operator
            .and_then(|o| o.network.clone())
            .unwrap_or_else(|| "mainnet".to_string());
        let rpc_url = mpps
            .first()
            .map(|m| m.rpc_url().to_string())
            .unwrap_or_else(|| {
                pay_kit::mpp::protocol::solana::default_rpc_url(&network).to_string()
            });
        let fee_payer = operator.map(|o| o.fee_payer).unwrap_or(false);
        let signer = self.state.fee_payer_signer();
        let csec = operator.and_then(|o| o.challenge_binding_secret.as_deref());
        let realm = operator
            .and_then(|o| o.realm.as_deref())
            .or(Some(subdomain));
        let canonical = format!("https://{subdomain}/");
        let defaults = sub::OperatorDefaults {
            puller: &puller,
            recipient: &recipient,
            network: &network,
            rpc_url: &rpc_url,
            challenge_binding_secret: csec,
            realm,
            fee_payer,
            fee_payer_signer: signer.clone(),
        };

        // Build the 402: subscription challenge + optional authenticate challenge.
        let challenge_402 = |error: Option<(&str, bool)>| -> GateDecision {
            let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
            match sub::build_challenge(spec, defaults.clone(), description) {
                Ok(c) => {
                    if let Ok(w) = format_www_authenticate(&c)
                        && let Ok(v) = HeaderValue::from_str(&w)
                    {
                        headers.push((header::WWW_AUTHENTICATE, v));
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "subscription challenge generation failed");
                    return GateDecision::Respond(GateResponse::json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Bytes::from_static(br#"{"error":"subscription_misconfigured"}"#),
                    ));
                }
            }
            if let Ok(Some(authsrv)) =
                authenticate::build_handler(spec, defaults.clone(), subdomain, &canonical)
                && let Ok(ac) = authsrv.challenge()
                && let Ok(w) = format_www_authenticate(&ac)
                && let Ok(v) = HeaderValue::from_str(&w)
            {
                headers.push((header::WWW_AUTHENTICATE, v));
            }
            telemetry::record_402_challenge_sent(
                "mpp-subscription",
                subdomain,
                path,
                req.method.as_str(),
                None,
                "subscription",
                1,
            );
            let body = match error {
                Some((m, retryable)) => {
                    json!({"error":"verification_failed","message":m,"retryable":retryable})
                }
                None => json!({
                    "error": "payment_required",
                    "message": "This endpoint requires a subscription.",
                    "endpoint": { "method": req.method.as_str(), "path": path },
                }),
            };
            let mut resp = GateResponse::json(
                StatusCode::PAYMENT_REQUIRED,
                serde_json::to_vec(&body).unwrap_or_default(),
            );
            resp.headers.extend(headers);
            GateDecision::Respond(resp)
        };

        let Some(auth) = req.authorization else {
            return challenge_402(None);
        };
        let credential = match parse_authorization(auth) {
            Ok(c) => c,
            Err(e) => {
                return GateDecision::Respond(GateResponse::json(
                    StatusCode::BAD_REQUEST,
                    serde_json::to_vec(
                        &json!({"error":"malformed_credential","message":e.to_string()}),
                    )
                    .unwrap_or_default(),
                ));
            }
        };

        // authenticate-intent: stateless SIWMPP verify, no broadcast.
        if credential.challenge.intent.as_str() == "authenticate" {
            if let Ok(Some(server)) =
                authenticate::build_handler(spec, defaults.clone(), subdomain, &canonical)
                && server.verify(&credential).is_ok()
            {
                return GateDecision::Forward {
                    session: None,
                    receipt: None,
                };
            }
            return challenge_402(None);
        }
        if credential.challenge.intent.as_str() != "subscription" {
            return challenge_402(None);
        }

        // subscription-intent: activation (broadcasts).
        let server = match sub::build_handler(spec, defaults.clone(), description) {
            Ok(s) => s,
            Err(e) => {
                return GateDecision::Respond(GateResponse::json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::to_vec(
                        &json!({"error":"subscription_misconfigured","message":e.to_string()}),
                    )
                    .unwrap_or_default(),
                ));
            }
        };
        match server.verify_credential(&credential).await {
            Ok(receipt_kind) => {
                let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
                if let Ok(rs) = format_receipt(&receipt_kind)
                    && let Ok(v) = HeaderValue::from_str(&rs)
                {
                    headers.push((HeaderName::from_static(PAYMENT_RECEIPT_HEADER), v));
                }
                if let Ok(Some(authsrv)) =
                    authenticate::build_handler(spec, defaults.clone(), subdomain, &canonical)
                    && let Ok(ac) = authsrv.challenge()
                    && let Ok(w) = format_www_authenticate(&ac)
                    && let Ok(v) = HeaderValue::from_str(&w)
                {
                    headers.push((header::WWW_AUTHENTICATE, v));
                }
                GateDecision::Forward {
                    session: None,
                    receipt: Some(ReceiptAnnotation {
                        headers,
                        reference: Some(receipt_kind.base().reference.clone()),
                    }),
                }
            }
            Err(e) => {
                telemetry::record_settlement_error(
                    "mpp-subscription",
                    subdomain,
                    path,
                    &e.message,
                    e.retryable,
                );
                challenge_402(Some((&e.message, e.retryable)))
            }
        }
    }

    /// Verify an MPP `charge` credential across the configured MPPs. On success,
    /// forward with a receipt; on failure, re-challenge with 402.
    #[allow(clippy::too_many_arguments)]
    async fn charge_verify(
        &self,
        api: &pay_types::metering::ApiSpec,
        meter: &pay_types::metering::Metering,
        description: Option<&str>,
        auth: &str,
        subdomain: &str,
        path: &str,
        req: &GateRequest<'_>,
    ) -> GateDecision {
        let mpps = self.state.mpps();
        if mpps.is_empty() {
            return GateDecision::Respond(GateResponse::json(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::to_vec(&json!({
                    "error": "payment_backend_unconfigured",
                    "message": "This endpoint requires payment, but no payment backend is configured.",
                }))
                .unwrap_or_default(),
            ));
        }
        let credential = match parse_authorization(auth) {
            Ok(c) => c,
            Err(e) => {
                return GateDecision::Respond(GateResponse::json(
                    StatusCode::BAD_REQUEST,
                    serde_json::to_vec(&json!({
                        "error": "malformed_credential", "message": e.to_string()
                    }))
                    .unwrap_or_default(),
                ));
            }
        };

        let props = metering::RequestProperties {
            body_size: req.content_length,
            ..Default::default()
        };
        let variant = variant_hint_from_path(path);
        let amount = crate::server::payment::charge_amount_from_price(
            metering::resolve_price(meter, &props, variant.as_deref(), None).as_ref(),
        );
        // Reconstruct a URI for split-rule query params (splits price off the request).
        let uri: http::Uri = format!(
            "/{}{}",
            path,
            req.query.map(|q| format!("?{q}")).unwrap_or_default()
        )
        .parse()
        .unwrap_or_default();

        let mut last_error = None;
        for mpp in &mpps {
            // Audit: verify against the challenge WE would issue (rebuilt from our
            // own price + splits), not the values echoed in the credential.
            let expected = match mpp.charge_with_options(
                &amount,
                ChargeOptions {
                    description,
                    splits: crate::server::payment::resolve_charge_splits(
                        mpp, meter, api, &uri, &amount,
                    ),
                    ..Default::default()
                },
            ) {
                Ok(ch) => match ch.request.decode() {
                    Ok(r) => r,
                    Err(e) => {
                        last_error = Some(VerificationError::new(format!(
                            "decode expected charge: {e}"
                        )));
                        continue;
                    }
                },
                Err(e) => {
                    last_error = Some(VerificationError::new(format!(
                        "rebuild expected charge: {e}"
                    )));
                    continue;
                }
            };
            match mpp
                .verify_credential_with_expected(&credential, &expected)
                .await
            {
                Ok(receipt) => {
                    let reference = receipt.reference.clone();
                    let payment = crate::server::payment::decode_payment_amount(
                        &credential,
                        mpp.decimals() as u8,
                    );
                    telemetry::record_payment_collected(
                        "mpp",
                        subdomain,
                        path,
                        payment.as_ref(),
                        &reference,
                    );
                    if let Some(wallet) = self.state.fee_payer_wallet().cloned() {
                        let (sd, p) = (subdomain.to_string(), path.to_string());
                        tokio::spawn(async move {
                            wallet.observe("payment_verified", &sd, &p).await;
                        });
                    }
                    let mut headers = Vec::new();
                    if let Some(url) = crate::explorer::tx_url(mpp.network(), &reference)
                        && let Ok(v) = HeaderValue::from_str(&url)
                    {
                        headers.push((PAYMENT_RECEIPT_URL, v));
                    }
                    if let Ok(rh) = format_receipt(&ReceiptKind::Charge(receipt))
                        && let Ok(v) = HeaderValue::from_str(&rh)
                    {
                        headers.push((HeaderName::from_static(PAYMENT_RECEIPT_HEADER), v));
                    }
                    return GateDecision::Forward {
                        session: None,
                        receipt: Some(ReceiptAnnotation {
                            headers,
                            reference: Some(reference),
                        }),
                    };
                }
                Err(e) => last_error = Some(e),
            }
        }

        let error = last_error.unwrap_or_else(|| VerificationError::new("MPP not configured"));
        let message = crate::server::payment::readable_verification_message(&error);
        telemetry::record_settlement_error("mpp", subdomain, path, &message, error.retryable);
        GateDecision::Respond(GateResponse::json(
            StatusCode::PAYMENT_REQUIRED,
            serde_json::to_vec(&json!({
                "error": "verification_failed",
                "message": message,
                "retryable": error.retryable,
            }))
            .unwrap_or_default(),
        ))
    }
}

/// Reconstruct a minimal URI from path + query for split-rule resolution.
fn reconstruct_uri(path: &str, query: Option<&str>) -> http::Uri {
    format!(
        "/{}{}",
        path,
        query.map(|q| format!("?{q}")).unwrap_or_default()
    )
    .parse()
    .unwrap_or_default()
}

/// Settlement reference from a verified x402 payment (`None` if unsigned).
fn x402_reference(verified: &VerifiedExactPayment) -> Option<String> {
    let reference = match verified {
        VerifiedExactPayment::Signature(sig) => sig.clone(),
        VerifiedExactPayment::Transaction(tx) => tx
            .signatures
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default(),
    };
    (!reference.is_empty()).then_some(reference)
}

/// Path-only variant hint (e.g. `/models/{name}:action` → `name`).
fn variant_hint_from_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if (*part == "models" || *part == "voices")
            && let Some(next) = parts.get(i + 1)
        {
            return Some(next.split(':').next().unwrap_or(next).to_string());
        }
    }
    None
}

/// Process a session credential and map the outcome to a [`GateDecision`].
async fn session_authorized(
    sm: &SessionMpp,
    handle: Option<Arc<SessionMpp>>,
    auth: &str,
    subdomain: &str,
    path: &str,
) -> GateDecision {
    match sm.process(auth).await {
        Ok(SessionOutcome::Active(state)) => GateDecision::Forward {
            session: handle.map(|h| SessionForward {
                handle: h,
                channel_id: state.channel_id,
                committed_base_units: state.cumulative,
            }),
            receipt: None,
        },
        Ok(SessionOutcome::Voucher {
            channel_id,
            cumulative,
        }) => GateDecision::Forward {
            session: handle.map(|h| SessionForward {
                handle: h,
                channel_id,
                committed_base_units: cumulative,
            }),
            receipt: None,
        },
        Ok(SessionOutcome::Commit(receipt)) => {
            telemetry::record_paid_request_completed(
                "session",
                subdomain,
                path,
                StatusCode::OK,
                None,
            );
            GateDecision::Respond(GateResponse::json(
                StatusCode::OK,
                serde_json::to_vec(&receipt).unwrap_or_default(),
            ))
        }
        Ok(SessionOutcome::Closed { signature, .. }) => {
            let receipt_url = signature
                .as_deref()
                .and_then(|s| crate::explorer::tx_url(sm.network(), s));
            let body = json!({
                "status": "closed",
                "signature": signature,
                "transactionId": signature,
                "receiptUrl": receipt_url,
            });
            let mut resp = GateResponse::json(
                StatusCode::OK,
                serde_json::to_vec(&body).unwrap_or_default(),
            );
            if let Some(url) = receipt_url {
                resp = resp.header(PAYMENT_RECEIPT_URL, url);
            }
            GateDecision::Respond(resp)
        }
        Err(e) => {
            telemetry::record_settlement_error("session", subdomain, path, &e.to_string(), true);
            GateDecision::Respond(GateResponse::json(
                StatusCode::PAYMENT_REQUIRED,
                serde_json::to_vec(&json!({
                    "error": "session_failed",
                    "message": e.to_string(),
                }))
                .unwrap_or_default(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(method: &'a Method, path: &'a str) -> GateRequest<'a> {
        GateRequest {
            method,
            path,
            host: Some("api.example.com"),
            accept: None,
            authorization: None,
            content_length: None,
            query: None,
            x402_payment: None,
        }
    }

    // A PaymentState with no APIs → everything is Passthrough.
    #[derive(Clone)]
    struct EmptyState;
    impl PaymentState for EmptyState {
        fn apis(&self) -> &[pay_types::metering::ApiSpec] {
            &[]
        }
        fn mpp(&self) -> Option<&pay_kit::mpp::server::Mpp> {
            None
        }
    }

    #[tokio::test]
    async fn discovery_and_control_plane_passthrough() {
        let gate = PaymentGate::new(EmptyState);
        for path in [
            "__402/health",
            "openapi.json",
            ".well-known/pay-skills.json",
        ] {
            assert!(matches!(
                gate.evaluate(&req(&Method::GET, path)).await,
                GateDecision::Passthrough
            ));
        }
    }

    #[tokio::test]
    async fn unknown_subdomain_passthrough() {
        let gate = PaymentGate::new(EmptyState);
        assert!(matches!(
            gate.evaluate(&req(&Method::GET, "v1/anything")).await,
            GateDecision::Passthrough
        ));
    }
}
