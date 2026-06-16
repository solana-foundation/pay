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
    AUTHORIZATION_HEADER, PAYMENT_RECEIPT_HEADER, ReceiptKind, WWW_AUTHENTICATE_HEADER,
    format_receipt, format_www_authenticate_many, parse_authorization, server::html as mpp_html,
};
use std::sync::Arc;

use crate::PaymentState;
use crate::server::metering::{self, RequestProperties};
use crate::server::session_stream::SessionStreamContext;
use crate::server::subscription as server_subscription;
use crate::server::telemetry;

/// Small non-generic helpers keep the middleware readable and, importantly for
/// coverage tooling, avoid burying the critical payment branches inside one
/// monomorphized async state machine.
struct ChargeRequestContext<'a> {
    method: &'a Method,
    path: &'a str,
    uri: &'a axum::http::Uri,
    subdomain: &'a str,
    accepts_html: bool,
    browser_rpc_url: Option<&'a str>,
}

const PAYMENT_PAGE_CONTENT_SECURITY_POLICY: &str = "\
    default-src 'self'; \
    script-src 'unsafe-inline'; \
    style-src 'unsafe-inline'; \
    img-src 'self' data: blob: https:; \
    connect-src 'self' http://localhost:* http://127.0.0.1:* https:; \
    worker-src 'self'";

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

    // Discovery surfaces — must stay unauthenticated so local agents and
    // tooling can read them without paying. `/openapi.json` and
    // `/.well-known/pay-skills.json` are pay's discovery contract; the
    // RFC 8615 `.well-known/` prefix is reserved for unauthenticated
    // metadata so we exempt the whole subtree.
    if path == "openapi.json" || path.starts_with(".well-known/") {
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
    let subscription_config = endpoint.and_then(|ep| ep.subscription.as_ref());

    if metering_config.is_none() && subscription_config.is_none() {
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

    // Subscription endpoints branch off before any of the charge / session
    // plumbing — they speak `intent=subscription` end-to-end and reuse none
    // of the per-call metering machinery.
    if let Some(sub_spec) = subscription_config {
        let auth_header = headers
            .get(AUTHORIZATION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // Fall back to the first MPP's recipient + rpc_url when
        // `operator.recipient` is omitted — at startup pay-server
        // resolves both into the MPP config (via the same chain that
        // honours `--rpc-url`, `operator.rpc_url`, or the sandbox URL).
        // Surfacing them here keeps the subscription middleware
        // RPC-config-aware without duplicating the resolver.
        let mpps = state.mpps();
        let operator_recipient_fallback = mpps.first().map(|m| m.recipient().to_string());
        let operator_rpc_url = mpps
            .first()
            .map(|m| m.rpc_url().to_string())
            .unwrap_or_else(|| {
                solana_mpp::protocol::solana::default_rpc_url("mainnet").to_string()
            });
        let fee_payer_signer = state.fee_payer_signer();
        let endpoint_description = endpoint.and_then(|ep| ep.description.as_deref());
        return handle_subscription_endpoint(
            api,
            sub_spec,
            endpoint_description,
            auth_header.as_deref(),
            operator_recipient_fallback.as_deref(),
            &operator_rpc_url,
            fee_payer_signer,
            req,
            next,
            &method,
            &path,
            subdomain,
        )
        .await;
    }

    let meter = metering_config.unwrap();

    let props = extract_request_properties(&headers, &path);
    let variant_hint = extract_variant_hint(&path);

    let auth_header = headers
        .get(AUTHORIZATION_HEADER)
        .and_then(|v| v.to_str().ok());

    let session_mpp_handle = state.session_mpp_handle();
    let session_mpp = session_mpp_handle
        .as_deref()
        .or_else(|| state.session_mpp());

    if let Some(session_mpp) = session_mpp {
        if auth_header.is_none() {
            let price = metering::resolve_price(meter, &props, variant_hint.as_deref(), None);
            return session_challenge_response(session_mpp, &method, &path, subdomain, price);
        }

        if let Some(auth_value) = auth_header.filter(|value| is_session_authorization(value)) {
            return handle_session_authorization(
                session_mpp,
                session_mpp_handle.clone(),
                auth_value,
                subdomain,
                &path,
                req,
                next,
            )
            .await;
        }
    }

    let mpps = state.mpps();
    if mpps.is_empty() {
        // Fail closed (issue #373): this endpoint declares a price, but the
        // server has no MPP payment backend configured, so we can neither
        // issue a 402 challenge nor verify a credential. Forwarding to the
        // protected upstream would serve a paid resource for free, turning a
        // misconfiguration into a silent fail-open. Reject with a server error
        // and an error-level event instead.
        //
        // This does not affect local development: `pay --sandbox server start`
        // auto-configures an ephemeral MPP, and a non-sandbox boot without a
        // resolvable signer/recipient already fails at startup.
        tracing::error!(
            subdomain = %subdomain,
            path = %path,
            "Metered endpoint hit but no MPP payment backend is configured — failing closed",
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({
                "error": "payment_backend_unconfigured",
                "message": "This endpoint requires payment, but the server has no payment backend configured. Failing closed instead of serving the resource without payment.",
            })),
        )
            .into_response();
    }

    match auth_header {
        None => charge_challenge_response(
            &mpps,
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
                browser_rpc_url: state.browser_rpc_url(),
            },
            endpoint.and_then(|ep| ep.description.as_deref()),
        ),
        Some(auth_value) => {
            handle_charge_authorization(
                &mpps,
                auth_value,
                subdomain,
                &path,
                state.fee_payer_wallet().cloned(),
                req,
                next,
            )
            .await
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
    let amount_usd = price
        .as_ref()
        .and_then(|p| p.dimensions.first())
        .map(|d| d.price_usd / d.scale.max(1) as f64);
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
    telemetry::record_402_challenge_sent(
        "session",
        subdomain,
        path,
        method.as_str(),
        amount_usd,
        "session",
        1,
    );
    challenge_json_response(body, &[www_auth])
}

#[tracing::instrument(
    name = "session_authorization",
    skip(session_mpp, session_mpp_handle, auth_value, req, next),
    fields(subdomain = %subdomain, path = %path)
)]
async fn handle_session_authorization(
    session_mpp: &crate::server::session::SessionMpp,
    session_mpp_handle: Option<Arc<crate::server::session::SessionMpp>>,
    auth_value: &str,
    subdomain: &str,
    path: &str,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    match session_mpp.process(auth_value).await {
        Ok(outcome) => {
            use crate::server::session::SessionOutcome;
            match outcome {
                SessionOutcome::Active(state) => {
                    attach_session_stream_context(
                        session_mpp_handle,
                        &mut req,
                        state.channel_id.clone(),
                        state.cumulative,
                    );
                    tracing::info!(subdomain = %subdomain, path = %path, "Session action accepted — forwarding");
                    let response = next.run(req).await;
                    telemetry::record_paid_request_completed(
                        "session",
                        subdomain,
                        path,
                        response.status(),
                        None,
                    );
                    response
                }
                SessionOutcome::Voucher {
                    channel_id,
                    cumulative,
                } => {
                    attach_session_stream_context(
                        session_mpp_handle,
                        &mut req,
                        channel_id,
                        cumulative,
                    );
                    tracing::info!(subdomain = %subdomain, path = %path, "Voucher accepted — forwarding");
                    let response = next.run(req).await;
                    telemetry::record_paid_request_completed(
                        "session",
                        subdomain,
                        path,
                        response.status(),
                        None,
                    );
                    response
                }
                SessionOutcome::Commit(receipt) => {
                    tracing::info!(
                        subdomain = %subdomain,
                        path = %path,
                        delivery_id = %receipt.delivery_id,
                        "Session delivery committed"
                    );
                    telemetry::record_paid_request_completed(
                        "session",
                        subdomain,
                        path,
                        StatusCode::OK,
                        None,
                    );
                    (StatusCode::OK, axum::Json(receipt)).into_response()
                }
                SessionOutcome::Closed { signature, .. } => {
                    tracing::info!(subdomain = %subdomain, path = %path, "Session closed");
                    (
                        StatusCode::OK,
                        axum::Json(json!({
                            "status": "closed",
                            "signature": signature,
                            "transactionId": signature,
                        })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            telemetry::record_settlement_error("session", subdomain, path, &e.to_string(), true);
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

fn attach_session_stream_context(
    session_mpp: Option<Arc<crate::server::session::SessionMpp>>,
    req: &mut Request<Body>,
    session_id: String,
    committed_base_units: u64,
) {
    let Some(session_mpp) = session_mpp else {
        return;
    };
    req.extensions_mut().insert(SessionStreamContext::new(
        session_mpp,
        session_id,
        committed_base_units,
    ));
}

#[allow(clippy::too_many_arguments)]
async fn handle_subscription_endpoint(
    api: &pay_types::metering::ApiSpec,
    spec: &pay_types::metering::SubscriptionEndpoint,
    endpoint_description: Option<&str>,
    auth_header: Option<&str>,
    operator_recipient_fallback: Option<&str>,
    operator_rpc_url: &str,
    fee_payer_signer: Option<std::sync::Arc<dyn solana_mpp::solana_keychain::SolanaSigner>>,
    req: Request<Body>,
    next: Next,
    method: &Method,
    path: &str,
    subdomain: &str,
) -> Response {
    // No Authorization → emit a 402 with the subscription challenge.
    let Some(auth_value) = auth_header else {
        return subscription_challenge_response(
            api,
            spec,
            endpoint_description,
            operator_recipient_fallback,
            operator_rpc_url,
            fee_payer_signer.clone(),
            method,
            path,
            subdomain,
        );
    };

    let credential = match parse_authorization(auth_value) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({
                    "error": "malformed_credential",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    // ── authenticate-intent path: stateless SIWMPP verify, no broadcast ──
    //
    // Subscribers within a paid period present a cached authenticate
    // credential on every request; the server verifies the ed25519
    // signature + HMAC nonce + resource binding without touching chain
    // state, then passes the request upstream. Failure falls through
    // to the 402 path with a fresh authenticate challenge so the client
    // can re-sign on period rollover or after the realm secret rotates.
    if credential.challenge.intent.as_str() == "authenticate" {
        let operator = api.operator.as_ref();
        let puller = operator
            .and_then(|o| o.recipient.clone())
            .or_else(|| operator_recipient_fallback.map(str::to_string));
        let network = operator
            .and_then(|o| o.network.clone())
            .unwrap_or_else(|| "mainnet".to_string());
        let fee_payer = operator.map(|o| o.fee_payer).unwrap_or(false);

        if let Some(puller) = puller {
            let recipient = spec.recipient.clone().unwrap_or_else(|| puller.clone());
            let defaults = crate::server::subscription::OperatorDefaults {
                puller: &puller,
                recipient: &recipient,
                network: &network,
                rpc_url: operator_rpc_url,
                challenge_binding_secret: operator
                    .and_then(|o| o.challenge_binding_secret.as_deref()),
                realm: operator
                    .and_then(|o| o.realm.as_deref())
                    .or(Some(subdomain)),
                fee_payer,
                fee_payer_signer: fee_payer_signer.clone(),
            };
            let canonical_uri = format!("https://{subdomain}/");
            match crate::server::authenticate::build_handler(
                spec,
                defaults,
                subdomain,
                &canonical_uri,
            ) {
                Ok(Some(server)) => match server.verify(&credential) {
                    Ok(subscriber) => {
                        tracing::debug!(
                            subdomain = %subdomain,
                            path = %path,
                            subscriber = %subscriber,
                            "authenticate credential verified"
                        );
                        return next.run(req).await;
                    }
                    Err(e) => {
                        tracing::info!(
                            subdomain = %subdomain,
                            path = %path,
                            error = %e,
                            "authenticate verify failed — re-challenging"
                        );
                    }
                },
                Ok(None) => {
                    tracing::debug!(
                        subdomain = %subdomain,
                        "authenticate not configured (plan not published yet)"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to build AuthenticateServer");
                }
            }
        }
        // Fall through to the 402 with both challenges.
        return subscription_challenge_response(
            api,
            spec,
            endpoint_description,
            operator_recipient_fallback,
            operator_rpc_url,
            fee_payer_signer.clone(),
            method,
            path,
            subdomain,
        );
    }

    if credential.challenge.intent.as_str() != "subscription" {
        // Wrong intent — re-emit the 402 with the subscription challenge.
        tracing::info!(
            subdomain = %subdomain,
            path = %path,
            "Credential intent mismatch on subscription endpoint — re-challenging"
        );
        return subscription_challenge_response(
            api,
            spec,
            endpoint_description,
            operator_recipient_fallback,
            operator_rpc_url,
            fee_payer_signer.clone(),
            method,
            path,
            subdomain,
        );
    }

    // Rebuild the SDK SubscriptionServer for the endpoint and verify.
    let operator = api.operator.as_ref();
    let puller = match operator
        .and_then(|o| o.recipient.clone())
        .or_else(|| operator_recipient_fallback.map(str::to_string))
    {
        Some(p) => p,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({
                    "error": "subscription_misconfigured",
                    "message": "Server is missing operator.recipient; configure it in pay-demo.yaml.",
                })),
            )
                .into_response();
        }
    };
    let recipient = spec.recipient.clone().unwrap_or_else(|| puller.clone());
    let network = operator
        .and_then(|o| o.network.clone())
        .unwrap_or_else(|| "mainnet".to_string());
    let fee_payer = operator.map(|o| o.fee_payer).unwrap_or(false);

    let defaults = crate::server::subscription::OperatorDefaults {
        puller: &puller,
        recipient: &recipient,
        network: &network,
        rpc_url: operator_rpc_url,
        challenge_binding_secret: operator.and_then(|o| o.challenge_binding_secret.as_deref()),
        realm: operator
            .and_then(|o| o.realm.as_deref())
            .or(Some(subdomain)),
        fee_payer,
        fee_payer_signer: fee_payer_signer.clone(),
    };
    let server =
        match crate::server::subscription::build_handler(spec, defaults, endpoint_description) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({
                        "error": "subscription_misconfigured",
                        "message": e.to_string(),
                    })),
                )
                    .into_response();
            }
        };

    match server.verify_credential(&credential).await {
        Ok(receipt_kind) => {
            // Attach the Payment-Receipt header and forward upstream.
            tracing::info!(
                subdomain = %subdomain,
                path = %path,
                reference = %receipt_kind.base().reference,
                "Subscription activation verified — forwarding"
            );
            let mut response = next.run(req).await;
            if let Ok(receipt_str) = format_receipt(&receipt_kind)
                && let Ok(v) = axum::http::HeaderValue::from_str(&receipt_str)
            {
                response.headers_mut().insert(PAYMENT_RECEIPT_HEADER, v);
            }
            // Also include a fresh authenticate challenge in the 200
            // response as a "for next time" hint — the client signs it
            // during the same Touch ID session as activation and caches
            // the resulting credential. Avoids an extra 402 round-trip
            // on the first post-activation request.
            let canonical_uri = format!("https://{subdomain}/");
            let defaults = crate::server::subscription::OperatorDefaults {
                puller: &puller,
                recipient: &recipient,
                network: &network,
                rpc_url: operator_rpc_url,
                challenge_binding_secret: operator
                    .and_then(|o| o.challenge_binding_secret.as_deref()),
                realm: operator
                    .and_then(|o| o.realm.as_deref())
                    .or(Some(subdomain)),
                fee_payer,
                fee_payer_signer: fee_payer_signer.clone(),
            };
            if let Ok(Some(authsrv)) = crate::server::authenticate::build_handler(
                spec,
                defaults,
                subdomain,
                &canonical_uri,
            ) && let Ok(auth_challenge) = authsrv.challenge()
                && let Ok(www_auth) = solana_mpp::format_www_authenticate(&auth_challenge)
                && let Ok(v) = axum::http::HeaderValue::from_str(&www_auth)
            {
                // RFC 7235 allows multiple WWW-Authenticate headers;
                // clients harvest the one whose intent matches the
                // cached subscription.
                response.headers_mut().append(WWW_AUTHENTICATE_HEADER, v);
            }
            response
        }
        Err(e) => {
            telemetry::record_settlement_error(
                "mpp-subscription",
                subdomain,
                path,
                &e.message,
                e.retryable,
            );
            tracing::warn!(
                subdomain = %subdomain,
                path = %path,
                error = %e.message,
                "Subscription activation verification failed"
            );
            // Emit a fresh subscription challenge so the client can retry.
            let mut response = (
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(json!({
                    "error": "verification_failed",
                    "message": e.message,
                    "retryable": e.retryable,
                })),
            )
                .into_response();
            if let Ok(challenge) = crate::server::subscription::build_challenge(
                spec,
                crate::server::subscription::OperatorDefaults {
                    puller: &puller,
                    recipient: &recipient,
                    network: &network,
                    rpc_url: operator_rpc_url,
                    challenge_binding_secret: operator
                        .and_then(|o| o.challenge_binding_secret.as_deref()),
                    realm: operator
                        .and_then(|o| o.realm.as_deref())
                        .or(Some(subdomain)),
                    fee_payer,
                    fee_payer_signer: fee_payer_signer.clone(),
                },
                endpoint_description,
            ) && let Ok(www_auth) = solana_mpp::format_www_authenticate(&challenge)
                && let Ok(v) = axum::http::HeaderValue::from_str(&www_auth)
            {
                response.headers_mut().insert(WWW_AUTHENTICATE_HEADER, v);
            }
            response
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn subscription_challenge_response(
    api: &pay_types::metering::ApiSpec,
    spec: &pay_types::metering::SubscriptionEndpoint,
    endpoint_description: Option<&str>,
    operator_recipient_fallback: Option<&str>,
    operator_rpc_url: &str,
    fee_payer_signer: Option<std::sync::Arc<dyn solana_mpp::solana_keychain::SolanaSigner>>,
    method: &Method,
    path: &str,
    subdomain: &str,
) -> Response {
    let operator = api.operator.as_ref();

    let puller = match operator
        .and_then(|o| o.recipient.clone())
        .or_else(|| operator_recipient_fallback.map(str::to_string))
    {
        Some(p) => p,
        None => {
            tracing::error!(
                "Subscription endpoint requires `operator.recipient` to derive the puller pubkey"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({
                    "error": "subscription_misconfigured",
                    "message": "Server is missing operator.recipient; configure it in pay-demo.yaml.",
                })),
            )
                .into_response();
        }
    };

    let recipient = spec.recipient.clone().unwrap_or_else(|| puller.clone());
    let network = operator
        .and_then(|o| o.network.clone())
        .unwrap_or_else(|| "mainnet".to_string());
    let fee_payer = operator.map(|o| o.fee_payer).unwrap_or(false);

    let defaults = server_subscription::OperatorDefaults {
        puller: &puller,
        recipient: &recipient,
        network: &network,
        rpc_url: operator_rpc_url,
        challenge_binding_secret: operator.and_then(|o| o.challenge_binding_secret.as_deref()),
        realm: operator
            .and_then(|o| o.realm.as_deref())
            .or(Some(subdomain)),
        fee_payer,
        fee_payer_signer: fee_payer_signer.clone(),
    };

    let subscription_challenge =
        match server_subscription::build_challenge(spec, defaults.clone(), endpoint_description) {
            Ok(c) => c,
            Err(e) => {
                telemetry::record_challenge_error(
                    "mpp-subscription",
                    "subscription",
                    &e.to_string(),
                );
                tracing::error!(error = %e, "Failed to build subscription challenge");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({
                        "error": "subscription_challenge_failed",
                        "message": e.to_string(),
                    })),
                )
                    .into_response();
            }
        };

    // Authenticate challenge — optional, depends on the spec having a
    // published plan. Failure is non-fatal: a 402 with only the
    // subscription challenge is still a valid response.
    let canonical_uri = format!("https://{subdomain}/");
    let authenticate_challenge =
        match crate::server::authenticate::build_handler(spec, defaults, subdomain, &canonical_uri)
        {
            Ok(Some(server)) => match server.challenge() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::debug!(error = %e, "Skipping authenticate challenge");
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::debug!(error = %e, "Skipping authenticate challenge");
                None
            }
        };

    let www_auth_sub = match solana_mpp::format_www_authenticate(&subscription_challenge) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "Failed to format subscription challenge");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "internal_error"})),
            )
                .into_response();
        }
    };

    let mut www_auths: Vec<String> = vec![www_auth_sub];
    if let Some(auth_challenge) = authenticate_challenge
        && let Ok(h) = solana_mpp::format_www_authenticate(&auth_challenge)
    {
        www_auths.push(h);
    }

    let amount = match server_subscription::resolve_amount(spec) {
        Ok((amt, _, _)) => amt,
        Err(e) => {
            telemetry::record_challenge_error("mpp-subscription", "subscription", &e.to_string());
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({
                    "error": "subscription_misconfigured",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    let body = json!({
        "error": "payment_required",
        "message": "This endpoint requires an active subscription.",
        "endpoint": { "method": method.as_str(), "path": path },
        "subscription": {
            "period": spec.period,
            "amount_base_units": amount,
            "currency": spec.currency,
            "plan_id": spec.plan_id,
            "expires_at": spec.expires_at,
        },
        "payment": {
            "protocol": "mpp",
            "intent": "subscription",
        },
    });

    telemetry::record_402_challenge_sent(
        "mpp-subscription",
        subdomain,
        path,
        method.as_str(),
        amount.parse::<f64>().ok(),
        &spec.currency,
        1,
    );

    challenge_json_response(body, &www_auths)
}

fn charge_challenge_response(
    mpps: &[&solana_mpp::server::Mpp],
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
        .map(|d| {
            let per_unit = d.price_usd / d.scale.max(1) as f64;
            format!("{}", per_unit)
        })
        .unwrap_or_else(|| "0.01".to_string());
    let mut challenges = Vec::with_capacity(mpps.len());
    for mpp in mpps {
        let splits = resolve_charge_splits(mpp, meter, api, request.uri, &amount);
        match mpp.charge_with_options(
            &amount,
            solana_mpp::server::ChargeOptions {
                description,
                splits,
                ..Default::default()
            },
        ) {
            Ok(challenge) => challenges.push(challenge),
            Err(e) => {
                telemetry::record_challenge_error("mpp", mpp.currency(), &e.to_string());
                tracing::error!(error = %e, currency = %mpp.currency(), "Failed to generate challenge");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(
                        json!({"error": "challenge_generation_failed", "message": e.to_string()}),
                    ),
                )
                    .into_response();
            }
        }
    }

    let www_auths = match format_www_authenticate_many(&challenges) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to format challenges");
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
        "payment": {
            "protocol": "mpp",
            "challenges": challenges.len(),
        },
    });

    let currencies = mpps
        .iter()
        .map(|mpp| mpp.currency())
        .collect::<Vec<_>>()
        .join(",");
    telemetry::record_402_challenge_sent(
        "mpp",
        request.subdomain,
        request.path,
        request.method.as_str(),
        amount.parse::<f64>().ok(),
        &currencies,
        challenges.len(),
    );

    if request.accepts_html {
        return html_challenge_response(
            mpps[0],
            &challenges[0],
            &www_auths,
            request.browser_rpc_url,
        );
    }

    challenge_json_response(body, &www_auths)
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

#[tracing::instrument(
    name = "charge_authorization",
    skip(mpps, auth_value, fee_payer_wallet, req, next),
    fields(subdomain = %subdomain, path = %path)
)]
async fn handle_charge_authorization(
    mpps: &[&solana_mpp::server::Mpp],
    auth_value: &str,
    subdomain: &str,
    path: &str,
    fee_payer_wallet: Option<telemetry::FeePayerWallet>,
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

    let mut last_error = None;
    for mpp in mpps {
        match mpp.verify_credential(&credential).await {
            Ok(receipt) => {
                let payment = decode_payment_amount(&credential, mpp.decimals() as u8);
                telemetry::record_payment_collected(
                    "mpp",
                    subdomain,
                    path,
                    payment.as_ref(),
                    &receipt.reference,
                );
                tracing::info!(subdomain = %subdomain, path = %path, reference = %receipt.reference, "Payment verified — forwarding");
                let mut response = next.run(req).await;
                let status = response.status();
                telemetry::record_paid_request_completed(
                    "mpp",
                    subdomain,
                    path,
                    status,
                    payment.as_ref(),
                );
                if let Some(wallet) = fee_payer_wallet.clone() {
                    let subdomain = subdomain.to_string();
                    let path = path.to_string();
                    tokio::spawn(async move {
                        wallet.observe("payment_verified", &subdomain, &path).await;
                    });
                }
                let kind = ReceiptKind::Charge(receipt);
                if let Ok(receipt_str) = format_receipt(&kind)
                    && let Ok(v) = axum::http::HeaderValue::from_str(&receipt_str)
                {
                    response.headers_mut().insert(PAYMENT_RECEIPT_HEADER, v);
                }
                return response;
            }
            Err(e) => last_error = Some(e),
        }
    }

    let error = last_error
        .unwrap_or_else(|| solana_mpp::server::VerificationError::new("MPP not configured"));
    let message = readable_verification_message(&error);
    telemetry::record_settlement_error("mpp", subdomain, path, &message, error.retryable);
    tracing::warn!(subdomain = %subdomain, path = %path, error = %message, "Payment verification failed");
    verification_failed_response(mpps, &error)
}

fn decode_payment_amount(
    credential: &solana_mpp::PaymentCredential,
    decimals: u8,
) -> Option<telemetry::PaymentAmount> {
    let request: solana_mpp::ChargeRequest = credential.challenge.request.decode().ok()?;
    telemetry::payment_amount_from_raw(&request.amount, decimals, request.currency)
}

fn verification_failed_response(
    mpps: &[&solana_mpp::server::Mpp],
    error: &solana_mpp::server::VerificationError,
) -> Response {
    let message = readable_verification_message(error);
    let mut response = (
        StatusCode::PAYMENT_REQUIRED,
        axum::Json(json!({
            "error": "verification_failed",
            "message": message,
            "retryable": error.retryable,
        })),
    )
        .into_response();
    let challenges: Vec<_> = mpps
        .iter()
        .filter_map(|mpp| mpp.charge("0.01").ok())
        .collect();
    if let Ok(www_auths) = format_www_authenticate_many(&challenges) {
        append_www_authenticate_headers(response.headers_mut(), &www_auths);
    }
    response
}

pub fn readable_verification_message(error: &solana_mpp::server::VerificationError) -> String {
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

fn challenge_json_response(body: serde_json::Value, www_auths: &[String]) -> Response {
    let mut response = (StatusCode::PAYMENT_REQUIRED, axum::Json(body)).into_response();
    append_www_authenticate_headers(response.headers_mut(), www_auths);
    response
}

fn html_challenge_response(
    mpp: &solana_mpp::server::Mpp,
    challenge: &solana_mpp::PaymentChallenge,
    www_auths: &[String],
    browser_rpc_url: Option<&str>,
) -> Response {
    let rpc_url = browser_rpc_url.unwrap_or_else(|| mpp.rpc_url());
    let page = mpp_html::challenge_to_html(challenge, rpc_url, mpp.network());
    tracing::info!(html_len = page.len(), "Generated HTML payment page");
    let mut response = Response::builder()
        .status(StatusCode::PAYMENT_REQUIRED)
        .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(
            axum::http::header::CONTENT_SECURITY_POLICY,
            PAYMENT_PAGE_CONTENT_SECURITY_POLICY,
        )
        .body(Body::from(page))
        .unwrap();
    append_www_authenticate_headers(response.headers_mut(), www_auths);
    response
}

fn append_www_authenticate_headers(headers: &mut HeaderMap, www_auths: &[String]) {
    for www_auth in www_auths {
        if let Ok(value) = axum::http::HeaderValue::from_str(www_auth) {
            headers.append(WWW_AUTHENTICATE_HEADER, value);
        }
    }
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
            challenge_binding_secret: Some("test-secret".to_string()),
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

    #[test]
    fn readable_verification_message_explains_fee_payer_authority_conflict() {
        let error = solana_mpp::server::VerificationError::invalid_payload(
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
        let error = solana_mpp::server::VerificationError::invalid_payload(
            "ATA creation owner is not authorized by the challenge",
        );
        let message = readable_verification_message(&error);
        assert_eq!(
            message,
            "Payment tried to create a token account this charge did not allow."
        );
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
        let header = solana_mpp::format_www_authenticate(&challenge).expect("header should format");
        let response = html_challenge_response(&mpp, &challenge, &[header], Some("/__402/rpc"));

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_TYPE),
            Some(&"text/html; charset=utf-8".parse().unwrap())
        );
        let csp = response
            .headers()
            .get(axum::http::header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .unwrap();
        assert!(csp.contains("img-src"));
        assert!(csp.contains("connect-src"));

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("<!doctype html>") || body.contains("<html"));
        assert!(body.contains("\"rpcUrl\":\"/__402/rpc\""));
    }
}
