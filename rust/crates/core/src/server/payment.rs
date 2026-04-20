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
    format_www_authenticate, parse_authorization, server::html as mpp_html,
};

use crate::PaymentState;
use crate::server::metering::{self, RequestProperties};

/// Small non-generic helpers keep the middleware readable and, importantly for
/// coverage tooling, avoid burying the critical payment branches inside one
/// monomorphized async state machine.
struct ChargeRequestContext<'a> {
    method: &'a Method,
    path: &'a str,
    uri: &'a axum::http::Uri,
    subdomain: &'a str,
    accepts_html: bool,
}

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

    if path.starts_with("__402/") {
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
        .is_some_and(mpp_html::accepts_html);

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
    let match_method = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };

    let exact_match = metering::find_endpoint(api, match_method, &path);
    let endpoint = exact_match.or_else(|| {
        // Browser payment-link flow: users often arrive here via a plain GET
        // after following a redirect or opening a link, even when the paid API
        // endpoint itself is POST-only. We therefore keep the normal
        // method-aware match for API clients, but allow an HTML browser to fall
        // back to path-only endpoint resolution so we can still render the 402
        // payment page instead of treating the request as an unknown route.
        //
        // This is also why proxy routing overrides intentionally stay
        // path-based in `server/proxy.rs`: once we've decided this browser
        // request should be handled as the payment-link UI for `/some/path`, we
        // want it to inherit that endpoint's transport behavior even though the
        // browser did not use the endpoint's canonical HTTP method.
        if accepts_html {
            metering::find_endpoint_by_path(api, &path)
        } else {
            None
        }
    });
    let metering_config = endpoint.and_then(|ep| ep.metering.as_ref());

    if metering_config.is_none() {
        // For respond routing with no method match: if the path exists but
        // the method is wrong, return 404 (not pass-through, since there's
        // no upstream to handle it).
        if api.routing.is_respond()
            && exact_match.is_none()
            && metering::find_endpoint_by_path(api, &path).is_some()
        {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"error":"not_found","message":"method not allowed"}"#,
                ))
                .unwrap();
        }
        return next.run(req).await;
    }

    let meter = metering_config.unwrap();

    let props = extract_request_properties(&headers, &path);
    let variant_hint = extract_variant_hint(&path);

    let auth_header = headers
        .get(AUTHORIZATION_HEADER)
        .and_then(|v| v.to_str().ok());

    if let Some(session_mpp) = state.session_mpp() {
        if auth_header.is_none() {
            let price = metering::resolve_price(meter, &props, variant_hint.as_deref(), None);
            return session_challenge_response(session_mpp, &method, &path, subdomain, price);
        }

        if let Some(auth_value) = auth_header.filter(|value| is_session_authorization(value)) {
            return handle_session_authorization(
                session_mpp,
                auth_value,
                subdomain,
                &path,
                req,
                next,
            )
            .await;
        }
    }

    let mpp = match state.mpp() {
        Some(mpp) => mpp,
        None => {
            tracing::warn!("Metered endpoint hit but MPP not configured — passing through");
            return next.run(req).await;
        }
    };

    match auth_header {
        None => charge_challenge_response(
            mpp,
            meter,
            api,
            &props,
            variant_hint.as_deref(),
            ChargeRequestContext {
                method: &method,
                path: &path,
                uri: &uri,
                subdomain,
                accepts_html,
            },
            endpoint.and_then(|ep| ep.description.as_deref()),
        ),
        Some(auth_value) => {
            handle_charge_authorization(mpp, auth_value, subdomain, &path, req, next).await
        }
    }
}

fn is_session_authorization(auth_value: &str) -> bool {
    parse_authorization(auth_value)
        .ok()
        .map(|credential| credential.challenge.intent.as_str() == "session")
        .unwrap_or(false)
}

fn session_challenge_response(
    session_mpp: &crate::server::session::SessionMpp,
    method: &Method,
    path: &str,
    subdomain: &str,
    price: Option<metering::ResolvedPrice>,
) -> Response {
    let body = json!({
        "error": "payment_required",
        "message": "This endpoint requires a session payment.",
        "endpoint": { "method": method.as_str(), "path": path },
        "pricing": price,
    });
    let www_auth = match session_mpp.challenge_header(u64::MAX) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "Failed to generate session challenge");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "challenge_generation_failed"})),
            )
                .into_response();
        }
    };
    tracing::info!(subdomain = %subdomain, path = %path, "402 Session Required");
    challenge_json_response(body, &www_auth)
}

async fn handle_session_authorization(
    session_mpp: &crate::server::session::SessionMpp,
    auth_value: &str,
    subdomain: &str,
    path: &str,
    req: Request<Body>,
    next: Next,
) -> Response {
    match session_mpp.process(auth_value).await {
        Ok(outcome) => {
            use crate::server::session::SessionOutcome;
            match outcome {
                SessionOutcome::Active(_state) => {
                    tracing::info!(subdomain = %subdomain, path = %path, "Session action accepted — forwarding");
                    next.run(req).await
                }
                SessionOutcome::Voucher(_cumulative) => {
                    tracing::info!(subdomain = %subdomain, path = %path, "Voucher accepted — forwarding");
                    next.run(req).await
                }
                SessionOutcome::Closed(_params) => {
                    tracing::info!(subdomain = %subdomain, path = %path, "Session closed");
                    (StatusCode::OK, axum::Json(json!({"status": "closed"}))).into_response()
                }
            }
        }
        Err(e) => {
            tracing::warn!(subdomain = %subdomain, path = %path, error = %e, "Session action failed");
            (
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(json!({
                    "error": "session_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

fn charge_challenge_response(
    mpp: &solana_mpp::server::Mpp,
    meter: &pay_types::metering::Metering,
    api: &pay_types::metering::ApiSpec,
    props: &RequestProperties,
    variant_hint: Option<&str>,
    request: ChargeRequestContext<'_>,
    description: Option<&str>,
) -> Response {
    let price = metering::resolve_price(meter, props, variant_hint, None);
    let amount = price
        .as_ref()
        .and_then(|p| p.dimensions.first())
        .map(|d| format!("{}", d.price_usd))
        .unwrap_or_else(|| "0.01".to_string());
    let splits = resolve_charge_splits(mpp, meter, api, request.uri, &amount);

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
                "endpoint": { "method": request.method.as_str(), "path": request.path },
                "pricing": price,
            });

            tracing::info!(subdomain = %request.subdomain, path = %request.path, amount = %amount, "402 Payment Required");

            if request.accepts_html {
                return html_challenge_response(mpp, &challenge, &www_auth);
            }

            challenge_json_response(body, &www_auth)
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to generate challenge");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(
                    json!({"error": "challenge_generation_failed", "message": e.to_string()}),
                ),
            )
                .into_response()
        }
    }
}

fn resolve_charge_splits(
    mpp: &solana_mpp::server::Mpp,
    meter: &pay_types::metering::Metering,
    api: &pay_types::metering::ApiSpec,
    uri: &axum::http::Uri,
    amount: &str,
) -> Vec<solana_mpp::protocol::solana::Split> {
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
            .map(|split| solana_mpp::protocol::solana::Split {
                recipient: split.recipient,
                amount: split.amount.to_string(),
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

async fn handle_charge_authorization(
    mpp: &solana_mpp::server::Mpp,
    auth_value: &str,
    subdomain: &str,
    path: &str,
    req: Request<Body>,
    next: Next,
) -> Response {
    let credential = match parse_authorization(auth_value) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "Invalid Authorization header");
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "malformed_credential", "message": e.to_string()})),
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
            verification_failed_response(mpp, &e)
        }
    }
}

fn verification_failed_response(
    mpp: &solana_mpp::server::Mpp,
    error: &solana_mpp::server::VerificationError,
) -> Response {
    let mut response = (
        StatusCode::PAYMENT_REQUIRED,
        axum::Json(json!({
            "error": "verification_failed",
            "message": error.to_string(),
            "retryable": error.retryable,
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

fn challenge_json_response(body: serde_json::Value, www_auth: &str) -> Response {
    let mut response = (StatusCode::PAYMENT_REQUIRED, axum::Json(body)).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(www_auth) {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE_HEADER, value);
    }
    response
}

fn html_challenge_response(
    mpp: &solana_mpp::server::Mpp,
    challenge: &solana_mpp::PaymentChallenge,
    www_auth: &str,
) -> Response {
    let page = mpp_html::challenge_to_html(challenge, mpp.rpc_url(), mpp.network());
    tracing::info!(html_len = page.len(), "Generated HTML payment page");
    let mut response = Response::builder()
        .status(StatusCode::PAYMENT_REQUIRED)
        .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header("content-security-policy", "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src *; worker-src 'self'")
        .body(Body::from(page))
        .unwrap();
    if let Ok(value) = axum::http::HeaderValue::from_str(www_auth) {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE_HEADER, value);
    }
    response
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
    use solana_mpp::WWW_AUTHENTICATE_HEADER;
    use solana_mpp::server::Mpp;
    use solana_mpp::server::session::SessionConfig;

    fn test_mpp() -> Mpp {
        Mpp::new(solana_mpp::server::Config {
            recipient: solana_pubkey::Pubkey::new_unique().to_string(),
            currency: "USDC".to_string(),
            decimals: 6,
            network: "localnet".to_string(),
            rpc_url: Some("http://localhost:8899".to_string()),
            secret_key: Some("test-secret".to_string()),
            ..Default::default()
        })
        .expect("test MPP config should be valid")
    }

    fn test_session_mpp() -> crate::server::session::SessionMpp {
        crate::server::session::SessionMpp::new(
            SessionConfig {
                operator: solana_pubkey::Pubkey::new_unique().to_string(),
                recipient: solana_pubkey::Pubkey::new_unique().to_string(),
                max_cap: 5_000_000,
                currency: solana_pubkey::Pubkey::new_unique().to_string(),
                network: "localnet".to_string(),
                modes: vec![solana_mpp::SessionMode::Push, solana_mpp::SessionMode::Pull],
                ..SessionConfig::default()
            },
            "test-secret",
        )
    }

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
    fn is_session_authorization_ignores_invalid_headers() {
        assert!(!is_session_authorization("not a valid auth header"));
    }

    #[test]
    fn parse_query_params_keeps_missing_values() {
        let uri: axum::http::Uri = "/v1/test?foo=bar&empty&baz=qux".parse().unwrap();
        let params = parse_query_params(&uri);
        assert_eq!(params.get("foo"), Some(&"bar".to_string()));
        assert_eq!(params.get("empty"), Some(&"".to_string()));
        assert_eq!(params.get("baz"), Some(&"qux".to_string()));
    }

    #[tokio::test]
    async fn session_challenge_response_sets_session_header() {
        let response = session_challenge_response(
            &test_session_mpp(),
            &Method::POST,
            "v1/generate",
            "testapi",
            None,
        );
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);

        let header = response
            .headers()
            .get(WWW_AUTHENTICATE_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap();
        assert!(header.contains("intent=\"session\""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn html_challenge_response_sets_html_content_type() {
        let mpp = test_mpp();
        let challenge = mpp.charge("0.01").expect("challenge should build");
        let header = format_www_authenticate(&challenge).expect("header should format");
        let response = html_challenge_response(&mpp, &challenge, &header);

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_TYPE),
            Some(&"text/html; charset=utf-8".parse().unwrap())
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("<!doctype html>") || body.contains("<html"));
    }
}
