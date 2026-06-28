//! Payment middleware for the proxy.
//!
//! Intercepts requests to metered endpoints:
//! - No payment header → 402 with MPP challenge (WWW-Authenticate)
//! - Payment header → verify with solana-mpp, then forward upstream

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use pay_kit::mpp::AUTHORIZATION_HEADER;

use crate::PaymentState;
use crate::server::metering::{self, RequestProperties};
use crate::server::session_stream::SessionStreamContext;
use crate::server::telemetry;

/// Axum middleware that gates metered endpoints behind MPP payment.
pub async fn payment_middleware<S: PaymentState>(
    axum::extract::State(state): axum::extract::State<S>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let span = tracing::info_span!(
        "payment_middleware",
        tx_sig = tracing::field::Empty,
        receipt_url = tracing::field::Empty,
    );
    #[cfg(feature = "otel")]
    crate::server::otel::set_parent_from_headers(&span, req.headers());
    tracing::Instrument::instrument(gate_adapter(state, req, next), span).await
}

/// Thin axum adapter over the framework-agnostic [`crate::server::gate`]: build
/// a `GateRequest`, evaluate, and map the `GateDecision` back onto axum.
async fn gate_adapter<S: PaymentState>(state: S, req: Request<Body>, next: Next) -> Response {
    use crate::server::gate::{GateDecision, GateRequest, PaymentGate};

    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().trim_start_matches('/').to_string();

    let str_header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let authorization = str_header(AUTHORIZATION_HEADER);
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let query = uri.query().map(str::to_string);
    let x402_payment = str_header(pay_kit::x402::PAYMENT_SIGNATURE_HEADER)
        .or_else(|| str_header(pay_kit::x402::X402_V1_PAYMENT_HEADER));

    let gate_req = GateRequest {
        method: &method,
        path: &path,
        host: host.as_deref(),
        accept: accept.as_deref(),
        authorization: authorization.as_deref(),
        content_length,
        query: query.as_deref(),
        x402_payment: x402_payment.as_deref(),
    };
    let gate = PaymentGate::new(state.clone());
    match gate.evaluate(&gate_req).await {
        GateDecision::Respond(r) => {
            let mut builder = Response::builder().status(r.status);
            for (n, v) in &r.headers {
                builder = builder.header(n, v);
            }
            builder
                .body(Body::from(r.body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        GateDecision::Forward {
            session,
            receipt,
            upto,
        } => {
            let mut req = req;
            if let Some(sf) = session {
                req.extensions_mut().insert(SessionStreamContext::new(
                    sf.handle,
                    sf.channel_id,
                    sf.committed_base_units,
                ));
            }
            let mut response = next.run(req).await;
            // x402 `upto`: settle the opened channel *after* serving — debit the
            // metered amount on success, refund on failure.
            if let Some(uf) = upto {
                let served_ok = response.status().is_success();
                if let Some((n, v)) =
                    crate::server::gate::settle_upto(&state, *uf.open, served_ok).await
                {
                    response.headers_mut().append(n, v);
                }
            }
            if let Some(ann) = receipt {
                for (n, v) in ann.headers {
                    response.headers_mut().append(n, v);
                }
                if let Some(reference) = ann.reference {
                    tracing::Span::current().record("tx_sig", reference.as_str());
                }
            }
            response
        }
        GateDecision::Passthrough => next.run(req).await,
    }
}

/// Per-unit charge amount (USD, as a decimal string) derived from the
/// resolved price; falls back to "0.01" when no price is configured. Shared
/// by the 402-issuing and verify paths so the advertised and expected amounts
/// always match.
pub(crate) fn charge_amount_from_price(price: Option<&metering::ResolvedPrice>) -> String {
    price
        .and_then(|p| p.dimensions.first())
        .map(|d| {
            let per_unit = d.price_usd / d.scale.max(1) as f64;
            format!("{}", per_unit)
        })
        .unwrap_or_else(|| "0.01".to_string())
}

pub(crate) fn resolve_charge_splits(
    mpp: &pay_kit::mpp::server::Mpp,
    meter: &pay_types::metering::Metering,
    api: &pay_types::metering::ApiSpec,
    uri: &axum::http::Uri,
    amount: &str,
) -> Vec<pay_kit::mpp::protocol::solana::Split> {
    let split_rules = metering::resolve_split_rules(meter);
    if split_rules.is_empty() {
        return vec![];
    }

    let amount_f64: f64 = amount.parse().unwrap_or(0.0);
    let decimals = mpp.decimals() as u8;
    let query_params = parse_query_params(uri);

    match pay_types::splits::resolve_splits(
        split_rules,
        &api.recipients,
        amount_f64,
        decimals,
        &query_params,
    ) {
        Ok(resolved) => resolved
            .into_iter()
            .map(|split| pay_kit::mpp::protocol::solana::Split {
                recipient: split.recipient,
                amount: split.amount.to_string(),
                ata_creation_required: None,
                label: split.label,
                memo: split.memo,
            })
            .collect(),
        Err(e) => {
            tracing::debug!(error = %e, "Splits not resolved — omitting from challenge");
            vec![]
        }
    }
}

pub(crate) fn decode_payment_amount(
    credential: &pay_kit::mpp::PaymentCredential,
    decimals: u8,
) -> Option<telemetry::PaymentAmount> {
    let request: pay_kit::mpp::ChargeRequest = credential.challenge.request.decode().ok()?;
    telemetry::payment_amount_from_raw(&request.amount, decimals, request.currency)
}

pub fn readable_verification_message(error: &pay_kit::mpp::server::VerificationError) -> String {
    let message = error.to_string();
    if message.contains("Fee payer cannot authorize the SPL payment transfer") {
        return "Payment used the same account for the server and client. Restart the demo server, then retry the request.".to_string();
    }
    if message.contains("Fee payer token account cannot fund the SPL payment transfer") {
        return "Payment used the server account instead of the client account. Restart the demo server, then retry the request.".to_string();
    }
    if message.contains("ATA creation owner is not authorized by the challenge") {
        return "Payment tried to create a token account this charge did not allow.".to_string();
    }
    message
}

fn parse_query_params(uri: &axum::http::Uri) -> std::collections::HashMap<String, String> {
    uri.query()
        .map(|query| {
            query
                .split('&')
                .filter_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    Some((
                        parts.next()?.to_string(),
                        parts.next().unwrap_or("").to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn extract_request_properties(headers: &HeaderMap, _path: &str) -> RequestProperties {
    let body_size = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    RequestProperties {
        body_size,
        ..Default::default()
    }
}

pub(crate) fn extract_variant_hint(path: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extract_variant_hint_models() {
        assert_eq!(
            extract_variant_hint("v1/models/gemini-2.0-flash:generateContent"),
            Some("gemini-2.0-flash".to_string())
        );
    }

    #[test]
    fn extract_variant_hint_voices() {
        assert_eq!(
            extract_variant_hint("v1/voices/chirp-3-hd:synthesize"),
            Some("chirp-3-hd".to_string())
        );
    }

    #[test]
    fn extract_variant_hint_no_colon() {
        assert_eq!(
            extract_variant_hint("v1/models/gpt-4"),
            Some("gpt-4".to_string())
        );
    }

    #[test]
    fn extract_variant_hint_no_match() {
        assert_eq!(extract_variant_hint("v1/images/generate"), None);
    }

    #[test]
    fn extract_variant_hint_empty() {
        assert_eq!(extract_variant_hint(""), None);
    }

    #[test]
    fn extract_variant_hint_models_at_end() {
        // "models" is the last segment — no next segment
        assert_eq!(extract_variant_hint("v1/models"), None);
    }

    #[test]
    fn extract_request_properties_with_content_length() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", "12345".parse().unwrap());
        let props = extract_request_properties(&headers, "/v1/test");
        assert_eq!(props.body_size, Some(12345));
    }

    #[test]
    fn extract_request_properties_no_content_length() {
        let headers = HeaderMap::new();
        let props = extract_request_properties(&headers, "/v1/test");
        assert_eq!(props.body_size, None);
    }

    #[test]
    fn extract_request_properties_invalid_content_length() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", "not-a-number".parse().unwrap());
        let props = extract_request_properties(&headers, "/v1/test");
        assert_eq!(props.body_size, None);
    }

    #[test]
    fn parse_query_params_keeps_missing_values() {
        let uri: axum::http::Uri = "/v1/test?foo=bar&empty&baz=qux".parse().unwrap();
        let params = parse_query_params(&uri);
        assert_eq!(params.get("foo"), Some(&"bar".to_string()));
        assert_eq!(params.get("empty"), Some(&"".to_string()));
        assert_eq!(params.get("baz"), Some(&"qux".to_string()));
    }

    #[test]
    fn readable_verification_message_explains_fee_payer_authority_conflict() {
        let error = pay_kit::mpp::server::VerificationError::invalid_payload(
            "Fee payer cannot authorize the SPL payment transfer",
        );
        let message = readable_verification_message(&error);
        assert_eq!(
            message,
            "Payment used the same account for the server and client. Restart the demo server, then retry the request."
        );
    }

    #[test]
    fn readable_verification_message_explains_disallowed_ata_creation() {
        let error = pay_kit::mpp::server::VerificationError::invalid_payload(
            "ATA creation owner is not authorized by the challenge",
        );
        let message = readable_verification_message(&error);
        assert_eq!(
            message,
            "Payment tried to create a token account this charge did not allow."
        );
    }
}
