//! Payment middleware for the proxy.
//!
//! Intercepts requests to metered endpoints:
//! - No payment header → 402 with MPP challenge (WWW-Authenticate)
//! - Payment header → verify with solana-mpp, then forward upstream

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use solana_mpp::{
    AUTHORIZATION_HEADER, PAYMENT_RECEIPT_HEADER, WWW_AUTHENTICATE_HEADER, format_receipt,
    format_www_authenticate, parse_authorization,
    server::html as mpp_html,
};

use crate::PaymentState;
use crate::server::metering::{self, RequestProperties};

/// Axum middleware that gates metered endpoints behind MPP payment.
pub async fn payment_middleware<S: PaymentState>(
    axum::extract::State(state): axum::extract::State<S>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().trim_start_matches('/').to_string();

    if path.starts_with("__gateway/") {
        return next.run(req).await;
    }

    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let subdomain = host.split('.').next().unwrap_or("");

    let accepts_html = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| mpp_html::accepts_html(v));

    let apis = state.apis();
    let api = match apis.iter().find(|a| a.subdomain == subdomain) {
        Some(api) => api,
        // Single-API mode: if only one API is configured, use it regardless of subdomain
        None if apis.len() == 1 => &apis[0],
        None => return next.run(req).await,
    };

    // Service worker for HTML payment link UI — must run before metering
    // lookup so it works for any endpoint path regardless of method.
    let query = uri.query().unwrap_or("");
    if query.contains(mpp_html::SERVICE_WORKER_PARAM) {
        return Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/javascript")
            .header("service-worker-allowed", "/")
            .body(Body::from(mpp_html::service_worker_js()))
            .unwrap();
    }

    // HEAD should be gated the same as GET.
    let match_method = if method == Method::HEAD { "GET" } else { method.as_str() };

    let endpoint = metering::find_endpoint(api, match_method, &path)
        .or_else(|| {
            // Browser payment link: GET request to a POST endpoint.
            // Fall back to path-only match so the payment page renders.
            if accepts_html {
                metering::find_endpoint_by_path(api, &path)
            } else {
                None
            }
        });
    let metering_config = endpoint.and_then(|ep| ep.metering.as_ref());

    if metering_config.is_none() {
        return next.run(req).await;
    }

    let meter = metering_config.unwrap();
    let mpp = match state.mpp() {
        Some(mpp) => mpp,
        None => {
            tracing::warn!("Metered endpoint hit but MPP not configured — passing through");
            return next.run(req).await;
        }
    };

    let props = extract_request_properties(&headers, &path);
    let variant_hint = extract_variant_hint(&path);

    let auth_header = headers
        .get(AUTHORIZATION_HEADER)
        .and_then(|v| v.to_str().ok());

    match auth_header {
        None => {
            let price = metering::resolve_price(meter, &props, variant_hint.as_deref(), None);

            let amount = price
                .as_ref()
                .and_then(|p| p.dimensions.first())
                .map(|d| format!("{}", d.price_usd))
                .unwrap_or_else(|| "0.01".to_string());

            let description = endpoint.and_then(|ep| ep.description.as_deref());

            // Resolve payment splits from metering config.
            let split_rules = metering::resolve_split_rules(meter);
            let amount_f64: f64 = amount.parse().unwrap_or(0.0);
            let decimals = mpp.decimals() as u8;
            let splits = if !split_rules.is_empty() {
                let query_params: std::collections::HashMap<String, String> = uri
                    .query()
                    .map(|q| {
                        q.split('&')
                            .filter_map(|pair| {
                                let mut parts = pair.splitn(2, '=');
                                Some((parts.next()?.to_string(), parts.next().unwrap_or("").to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                match pay_types::splits::resolve_splits(
                    split_rules,
                    &api.recipients,
                    amount_f64,
                    decimals,
                    &query_params,
                ) {
                    Ok(resolved) => resolved
                        .into_iter()
                        .map(|s| solana_mpp::protocol::solana::Split {
                            recipient: s.recipient,
                            amount: s.amount.to_string(),
                            label: s.label,
                            memo: s.memo,
                        })
                        .collect(),
                    Err(e) => {
                        tracing::debug!(error = %e, "Splits not resolved — omitting from challenge");
                        vec![]
                    }
                }
            } else {
                vec![]
            };

            match mpp.charge_with_options(
                &amount,
                solana_mpp::server::ChargeOptions {
                    description,
                    splits,
                    ..Default::default()
                },
            ) {
                Ok(challenge) => {
                    let www_auth = match format_www_authenticate(&challenge) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to format challenge");
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(json!({"error": "internal_error"})),
                            )
                                .into_response();
                        }
                    };

                    let body = json!({
                        "error": "payment_required",
                        "message": "This endpoint requires payment.",
                        "endpoint": { "method": method.as_str(), "path": path },
                        "pricing": price,
                    });

                    tracing::info!(subdomain = %subdomain, path = %path, amount = %amount, "402 Payment Required");

                    if accepts_html {
                        // Browser — render HTML payment link page.
                        let page = mpp_html::challenge_to_html(
                            &challenge,
                            mpp.rpc_url(),
                            mpp.network(),
                        );
                        tracing::info!(html_len = page.len(), "Generated HTML payment page");
                        let mut resp = Response::builder()
                            .status(StatusCode::PAYMENT_REQUIRED)
                            .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
                            .header("content-security-policy", "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src *; worker-src 'self'")
                            .body(Body::from(page))
                            .unwrap();
                        if let Ok(v) = axum::http::HeaderValue::from_str(&www_auth) {
                            resp.headers_mut().insert(WWW_AUTHENTICATE_HEADER, v);
                        }
                        return resp;
                    }

                    // API client — JSON 402.
                    let mut resp = (StatusCode::PAYMENT_REQUIRED, axum::Json(body)).into_response();
                    if let Ok(v) = axum::http::HeaderValue::from_str(&www_auth) {
                        resp.headers_mut().insert(WWW_AUTHENTICATE_HEADER, v);
                    }
                    resp
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to generate challenge");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": "challenge_generation_failed", "message": e.to_string()})),
                    )
                        .into_response()
                }
            }
        }
        Some(auth_value) => {
            let credential = match parse_authorization(auth_value) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "Invalid Authorization header");
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(
                            json!({"error": "malformed_credential", "message": e.to_string()}),
                        ),
                    )
                        .into_response();
                }
            };

            match mpp.verify_credential(&credential).await {
                Ok(receipt) => {
                    tracing::info!(subdomain = %subdomain, path = %path, reference = %receipt.reference, "Payment verified — forwarding");
                    let mut response = next.run(req).await;
                    if let Ok(receipt_str) = format_receipt(&receipt)
                        && let Ok(v) = axum::http::HeaderValue::from_str(&receipt_str)
                    {
                        response.headers_mut().insert(PAYMENT_RECEIPT_HEADER, v);
                    }
                    response
                }
                Err(e) => {
                    tracing::warn!(subdomain = %subdomain, path = %path, error = %e, "Payment verification failed");
                    let mut response = (
                        StatusCode::PAYMENT_REQUIRED,
                        axum::Json(json!({
                            "error": "verification_failed",
                            "message": e.to_string(),
                            "retryable": e.retryable,
                        })),
                    )
                        .into_response();
                    if let Ok(challenge) = mpp.charge("0.01")
                        && let Ok(www_auth) = format_www_authenticate(&challenge)
                        && let Ok(v) = axum::http::HeaderValue::from_str(&www_auth)
                    {
                        response.headers_mut().insert(WWW_AUTHENTICATE_HEADER, v);
                    }
                    response
                }
            }
        }
    }
}

fn extract_request_properties(headers: &HeaderMap, _path: &str) -> RequestProperties {
    let body_size = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    RequestProperties {
        body_size,
        ..Default::default()
    }
}

fn extract_variant_hint(path: &str) -> Option<String> {
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
}
