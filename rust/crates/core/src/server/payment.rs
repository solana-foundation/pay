//! Payment middleware for the proxy.
//!
//! Intercepts requests to metered endpoints:
//! - No payment header → 402 with MPP challenge (WWW-Authenticate)
//! - Payment header → verify with solana-mpp, then forward upstream

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http_body::Body as _;
use pay_kit::mpp::AUTHORIZATION_HEADER;

use crate::PaymentState;
use crate::server::completion_stream::{self, DeliveryTask, StreamCompletion};
use crate::server::metering::{self, RequestProperties};
use crate::server::session_stream::{self, SessionStreamContext};
use crate::server::telemetry;

const MAX_METERED_MODEL_HINT_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Identity minted by the payment gate after caller-supplied internal headers
/// have been discarded. Keeping it in request extensions prevents the generic
/// proxy layer from having to trust values from the public header map.
#[derive(Clone, Debug, Default)]
pub struct TrustedPaymentIdentity {
    pub payer: Option<String>,
    pub channel_id: Option<String>,
    pub original_host: Option<String>,
}

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

    let mut req = req;
    // These headers are an internal trust boundary. Always discard caller
    // values before payment evaluation; only a successfully verified MPP
    // session may add them back below.
    strip_internal_identity_headers(req.headers_mut());
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = match crate::server::gate::gate_path(uri.path()) {
        Ok(path) => path.to_string(),
        Err(response) => return (response.status, response.body).into_response(),
    };

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
            batch,
            paid_request,
        } => {
            // Own durable cleanup before awaiting request preparation or the
            // handler: cancellation while queued must also release the voucher.
            let batch = batch.map(|forward| DeliveryTask::batch(state.clone(), *forward));
            let mut upto = upto;
            if let Some(plan) = upto
                .as_mut()
                .and_then(|forward| forward.settlement.as_mut())
            {
                let (restored, variant) = match prepare_upto_request_body(req, &path, plan).await {
                    Ok(result) => result,
                    Err(mut response) => {
                        if let Some(forward) = upto.take()
                            && let Some((name, value)) = crate::server::gate::settle_upto(
                                &state,
                                *forward.open,
                                0,
                                false,
                                forward.telemetry,
                            )
                            .await
                        {
                            response.headers_mut().append(name, value);
                        }
                        return response;
                    }
                };
                req = restored;
                plan.variant_hint = variant;
            }
            let mut delegated_session = None;
            let mut verified_payer = None;
            let mut verified_channel = None;
            if let Some(sf) = session {
                let mut sf = *sf;
                verified_payer = sf.verified_payer.clone();
                verified_channel = Some(sf.channel_id.clone());
                if sf.settlement.is_some() {
                    // Delegated sessions are settled from the completed
                    // response below. The client-voucher stream context waits
                    // for client commits and must not be installed here.
                    if sf
                        .metered_plan()
                        .is_some_and(|plan| plan.variant_hint.is_none())
                    {
                        let force_stream_usage = path.ends_with("chat/completions");
                        let (restored, body_variant) =
                            match prepare_metered_request_body(req, force_stream_usage).await {
                                Ok(result) => result,
                                Err(response) => return response,
                            };
                        req = restored;
                        if let Some(plan) = sf.metered_plan_mut() {
                            plan.variant_hint = body_variant;
                        }
                    }
                    delegated_session = Some(sf);
                } else {
                    req.extensions_mut().insert(SessionStreamContext::new(
                        sf.handle,
                        sf.channel_id,
                        sf.committed_base_units,
                    ));
                }
            }
            req.extensions_mut().insert(TrustedPaymentIdentity {
                payer: verified_payer,
                channel_id: verified_channel,
                original_host: host,
            });
            let deadline = delegated_session
                .as_ref()
                .and_then(|forward| forward.deadline());
            let forwarding = async move {
                if let Some(forward) = &delegated_session
                    && forward.require_active().await.is_err()
                {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                let response = next.run(req).await;
                match delegated_session {
                    Some(sf) => settle_axum_delegated_response(sf, response).await,
                    None => response,
                }
            };
            let mut response = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, forwarding).await {
                    Ok(response) => response,
                    Err(_) => return StatusCode::GATEWAY_TIMEOUT.into_response(),
                },
                None => forwarding.await,
            };
            // x402 `upto`: settle the opened channel *after* serving — debit the
            // metered amount on success, refund on failure.
            if let Some(uf) = upto {
                let served_ok = response.status().is_success();
                if let Some(plan) = uf.settlement {
                    if metering::upto_requires_response_body(
                        &plan.metering,
                        plan.variant_hint.as_deref(),
                    ) {
                        let limit = metering::upto_response_body_limit(&plan.metering);
                        let (mut parts, body) = response.into_parts();
                        match axum::body::to_bytes(body, limit).await {
                            Ok(bytes) => {
                                if let Some((n, v)) = crate::server::gate::settle_upto_metered(
                                    &state,
                                    *uf.open,
                                    plan,
                                    served_ok,
                                    &parts.headers,
                                    Some(&bytes),
                                    uf.telemetry,
                                )
                                .await
                                {
                                    parts.headers.append(n, v);
                                }
                                parts.headers.remove(header::CONTENT_LENGTH);
                                response = Response::from_parts(parts, Body::from(bytes));
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "failed to buffer x402 upto response body; refunding"
                                );
                                let mut builder =
                                    Response::builder().status(StatusCode::BAD_GATEWAY);
                                if let Some((n, v)) = crate::server::gate::settle_upto(
                                    &state,
                                    *uf.open,
                                    0,
                                    false,
                                    uf.telemetry,
                                )
                                .await
                                {
                                    builder = builder.header(n, v);
                                }
                                response = builder
                                    .header(header::CONTENT_TYPE, "application/json")
                                    .body(Body::from(r#"{"error":"response_metering_failed"}"#))
                                    .unwrap_or_else(|_| {
                                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                                    });
                            }
                        }
                    } else if let Some((n, v)) = crate::server::gate::settle_upto_metered(
                        &state,
                        *uf.open,
                        plan,
                        served_ok,
                        response.headers(),
                        None,
                        uf.telemetry,
                    )
                    .await
                    {
                        response.headers_mut().append(n, v);
                    }
                } else if let Some((n, v)) = crate::server::gate::settle_upto(
                    &state,
                    *uf.open,
                    uf.settle_amount,
                    served_ok,
                    uf.telemetry,
                )
                .await
                {
                    response.headers_mut().append(n, v);
                }
            }
            // x402 `batch-settlement`: the voucher commits only now, and only
            // for a response that actually served. A failure drops the outcome,
            // leaving the client uncharged and free to retry it.
            if let Some(bf) = batch {
                let mut served_ok = response.status().is_success();
                let mut cached = None;
                let completion = response.extensions().get::<StreamCompletion>().cloned();
                if served_ok && let Some(completion) = completion {
                    response = completion_stream::response(response, completion, bf, true);
                } else {
                    if served_ok {
                        let cacheable = response.body().size_hint().upper().is_some_and(|length| {
                            length <= crate::server::gate::MAX_BATCH_CACHED_RESPONSE_BYTES as u64
                        });
                        if cacheable {
                            let (mut parts, body) = response.into_parts();
                            match axum::body::to_bytes(
                                body,
                                crate::server::gate::MAX_BATCH_CACHED_RESPONSE_BYTES,
                            )
                            .await
                            {
                                Ok(bytes) => {
                                    cached = Some(crate::server::gate::batch_cached_response(
                                        parts.status,
                                        &parts.headers,
                                        &bytes,
                                    ));
                                    parts.headers.remove(header::CONTENT_LENGTH);
                                    response = Response::from_parts(parts, Body::from(bytes));
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        %error,
                                        "failed to buffer x402 batch response; releasing authorization"
                                    );
                                    served_ok = false;
                                    response = StatusCode::BAD_GATEWAY.into_response();
                                }
                            }
                        }
                    }
                    match bf.finish(served_ok, cached).await {
                        Ok(Some((n, v))) => {
                            response.headers_mut().append(n, v);
                        }
                        Ok(None) => {}
                        Err(_) => response = StatusCode::BAD_GATEWAY.into_response(),
                    }
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
            if let Some(paid_request) = paid_request {
                telemetry::record_paid_request_completed(
                    paid_request.protocol,
                    &paid_request.subdomain,
                    &path,
                    response.status(),
                    paid_request.payment.as_ref(),
                );
            }
            response
        }
        GateDecision::Passthrough => next.run(req).await,
    }
}

pub fn strip_internal_identity_headers(headers: &mut HeaderMap) {
    headers.remove("x-pay-verified-payer");
    headers.remove("x-pay-verified-channel");
    headers.remove("x-pay-original-host");
    headers.remove("x-pay-gateway-host");
    headers.remove("x-pay-wallet-resolver-proof");
    headers.remove("x-pay-payment-policy");
    headers.remove("x-pay-payment-policy-version");
}

/// Attach identity headers only after the payment gate has authenticated the
/// payer. Both HTTP data planes share this helper so an upstream cannot observe
/// caller-supplied identity at this internal trust boundary.
pub fn inject_verified_payer_headers(
    headers: &mut HeaderMap,
    payer: &str,
    original_host: Option<&str>,
    channel_id: Option<&str>,
) {
    if let Ok(value) = HeaderValue::from_str(payer) {
        headers.insert("x-pay-verified-payer", value);
        if let Some(channel_id) = channel_id
            && let Ok(value) = HeaderValue::from_str(channel_id)
        {
            headers.insert("x-pay-verified-channel", value);
        }
        if let Some(host) = original_host
            && let Ok(value) = HeaderValue::from_str(host)
        {
            headers.insert("x-pay-original-host", value);
        }
    }
}

/// Preserve the request host selected by the trusted edge after caller-supplied
/// internal headers have been removed. Resource routing needs this for every
/// accepted payment scheme, including x402 requests that have no session payer.
pub fn inject_original_host_header(headers: &mut HeaderMap, original_host: Option<&str>) {
    if let Some(host) = original_host
        && let Ok(value) = HeaderValue::from_str(host)
    {
        headers.insert("x-pay-original-host", value);
    }
}

/// Only inference requests need body preparation; unrelated upto uploads keep
/// their original streaming bodies and limits.
#[allow(clippy::result_large_err)]
async fn prepare_upto_request_body(
    request: Request<Body>,
    path: &str,
    plan: &metering::UptoSettlementPlan,
) -> Result<(Request<Body>, Option<String>), Response> {
    let chat = path == "chat/completions" || path.ends_with("/chat/completions");
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| {
            let mime = mime.trim().to_ascii_lowercase();
            mime == "application/json"
                || (mime.starts_with("application/") && mime.ends_with("+json"))
        });
    let needs_model = plan.variant_hint.is_none()
        && json
        && plan
            .metering
            .variants
            .iter()
            .any(|variant| variant.param == "model");
    // Non-model uploads must retain their streaming body and original limits.
    // Outside the known chat API, require an explicit JSON content type rather
    // than sniffing (and therefore consuming) an arbitrary request body.
    if !chat && !needs_model {
        return Ok((request, plan.variant_hint.clone()));
    }
    let (request, model) = prepare_metered_request_body(request, chat).await?;
    Ok((request, plan.variant_hint.clone().or(model)))
}

// A direct `Response` error lets the middleware short-circuit without losing
// status, headers, or body content produced while reading the request body.
#[allow(clippy::result_large_err)]
async fn prepare_metered_request_body(
    request: Request<Body>,
    force_stream_usage: bool,
) -> Result<(Request<Body>, Option<String>), Response> {
    let (mut parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_METERED_MODEL_HINT_BODY_BYTES)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to read metered request model");
            Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"request_body_too_large"}"#))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        })?;
    let (variant, bytes) = prepare_metered_json_body(&bytes, force_stream_usage);
    if force_stream_usage && let Ok(content_length) = bytes.len().to_string().parse() {
        parts.headers.insert(header::CONTENT_LENGTH, content_length);
    }
    Ok((Request::from_parts(parts, Body::from(bytes)), variant))
}

fn prepare_metered_json_body(body: &[u8], force_stream_usage: bool) -> (Option<String>, Vec<u8>) {
    let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, body.to_vec());
    };
    let variant = json
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);

    let is_stream = json
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !force_stream_usage || !is_stream {
        return (variant, body.to_vec());
    }

    let Some(object) = json.as_object_mut() else {
        return (variant, body.to_vec());
    };
    let stream_options = object
        .entry("stream_options")
        .or_insert_with(|| serde_json::json!({}));
    if !stream_options.is_object() {
        *stream_options = serde_json::json!({});
    }
    if let Some(stream_options) = stream_options.as_object_mut() {
        stream_options.insert("include_usage".to_string(), serde_json::Value::Bool(true));
    }

    (
        variant,
        serde_json::to_vec(&json).unwrap_or_else(|_| body.to_vec()),
    )
}

pub(super) async fn settle_axum_delegated_response(
    forward: crate::server::gate::SessionForward,
    response: Response,
) -> Response {
    if !response.status().is_success() && forward.deadline().is_none() {
        // Dropping `forward` releases the capacity lease without charging for
        // a response that was not successfully served.
        return response;
    }

    if forward.deadline().is_none()
        && forward
            .metered_plan()
            .is_some_and(|plan| metering::flat_request_price(&plan.metering).is_some())
        && let Some(completion) = response.extensions().get::<StreamCompletion>().cloned()
    {
        let delivery = DeliveryTask::session(forward, response.headers().clone());
        return completion_stream::response(response, completion, delivery, false);
    }

    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .any(|part| part.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    if crate::server::gate::is_streaming_response(response.headers())
        && forward.deadline().is_some()
    {
        // Fixed deployment charging is atomic on a completed response. Do not
        // release a partial stream that could later fail without being charged.
        return StatusCode::BAD_GATEWAY.into_response();
    }
    if is_sse && session_stream::DelegatedSessionStreamMeter::supports(&forward) {
        let (mut parts, body) = response.into_parts();
        let meter = match session_stream::DelegatedSessionStreamMeter::from_forward(forward) {
            Ok(meter) => meter,
            Err(error) => {
                tracing::error!(%error, "failed to configure delegated session stream metering");
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"error":"session_metering_failed"}"#))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        };
        parts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(
            parts,
            Body::from_stream(session_stream::meter_delegated_response_stream(
                body.into_data_stream(),
                meter,
                true,
            )),
        );
    }

    let limit = forward
        .metered_plan()
        .map(|plan| metering::upto_response_body_limit(&plan.metering))
        .unwrap_or(1024 * 1024);
    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "failed to buffer delegated session response body");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"response_metering_failed"}"#))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };

    if !parts.status.is_success() {
        parts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(parts, Body::from(bytes));
    }
    match crate::server::gate::settle_delegated_session(forward, &parts.headers, Some(&bytes)).await
    {
        Ok(receipt) => {
            if let Some(receipt) = receipt {
                for (name, value) in receipt.headers {
                    parts.headers.append(name, value);
                }
            }
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(bytes))
        }
        Err(error) => {
            tracing::error!(%error, "failed to settle delegated MPP session usage");
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"session_settlement_failed"}"#))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// Resolved per-unit price in token base units — the session challenge's
/// `amount` field (price per unit of service).
///
/// Converts the USD price by decimal scaling alone, so it is only sound for
/// USD-pegged settlement tokens (1 whole token == $1). Server startup
/// enforces that invariant by refusing non-pegged session currencies
/// (`ensure_session_currencies_usd_pegged` in the CLI server start path).
pub(crate) fn price_unit_base_amount(price: &metering::ResolvedPrice, decimals: u8) -> u64 {
    let per_unit = price
        .dimensions
        .first()
        .map(|d| d.price_usd / d.scale.max(1) as f64)
        .unwrap_or(0.01);
    ((per_unit * 10f64.powi(i32::from(decimals))).round() as u64).max(1)
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

const RESOURCE_MEMO_NONCE_HEX_LEN: usize = 3;
const RESOURCE_MEMO_TRUNC_HASH_HEX_LEN: usize = 6;
const RESOURCE_MEMO_TRUNC_SUFFIX_LEN: usize =
    1 + 1 + RESOURCE_MEMO_TRUNC_HASH_HEX_LEN + RESOURCE_MEMO_NONCE_HEX_LEN;

pub(crate) fn resource_memo_with_nonce(resource: Option<&str>, max_bytes: usize) -> Option<String> {
    let resource = resource.map(str::trim).filter(|r| !r.is_empty())?;
    let nonce = rand::random::<[u8; 2]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(RESOURCE_MEMO_NONCE_HEX_LEN)
        .collect::<String>();
    let memo = format!("{resource}#{nonce}");
    if memo.len() <= max_bytes {
        Some(memo)
    } else {
        let prefix_len = max_bytes.checked_sub(RESOURCE_MEMO_TRUNC_SUFFIX_LEN)?;
        let prefix = truncate_to_char_boundary(resource, prefix_len);
        if prefix.is_empty() {
            None
        } else {
            let hash = resource_memo_hash(resource);
            Some(format!("{prefix}#t{hash}{nonce}"))
        }
    }
}

pub(crate) fn resource_memo_matches(memo: &str, resource: &str, max_bytes: usize) -> bool {
    if memo == resource {
        return true;
    }
    let Some((prefix, suffix)) = memo.rsplit_once('#') else {
        return false;
    };
    if suffix.len() == RESOURCE_MEMO_NONCE_HEX_LEN
        && suffix.as_bytes().iter().all(u8::is_ascii_hexdigit)
        && prefix == resource
    {
        return true;
    }
    let Some(binding) = suffix.strip_prefix('t') else {
        return false;
    };
    if binding.len() != RESOURCE_MEMO_TRUNC_HASH_HEX_LEN + RESOURCE_MEMO_NONCE_HEX_LEN
        || !binding.as_bytes().iter().all(u8::is_ascii_hexdigit)
        || !binding.starts_with(&resource_memo_hash(resource))
    {
        return false;
    }
    let Some(expected_prefix_len) = max_bytes.checked_sub(RESOURCE_MEMO_TRUNC_SUFFIX_LEN) else {
        return false;
    };
    let expected_prefix = truncate_to_char_boundary(resource, expected_prefix_len);
    resource.len() > expected_prefix_len
        && !expected_prefix.is_empty()
        && prefix == expected_prefix
        && memo.len() <= max_bytes
}

fn resource_memo_hash(resource: &str) -> String {
    blake3::hash(resource.as_bytes()).to_hex()[..RESOURCE_MEMO_TRUNC_HASH_HEX_LEN].to_string()
}

fn truncate_to_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Whether `candidate` names the same token as `configured` on `network`:
/// the same string, or a symbol and a mint address that resolve to one mint.
pub fn same_currency(configured: &str, candidate: &str, network: &str) -> bool {
    // Token symbols are case-insensitive, but Solana base58 addresses are not.
    // The resolver below normalizes known symbols while leaving addresses exact.
    if configured == candidate {
        return true;
    }
    let network = Some(network);
    matches!(
        (
            pay_kit::mpp::resolve_stablecoin_mint(configured, network),
            pay_kit::mpp::resolve_stablecoin_mint(candidate, network),
        ),
        (Some(a), Some(b)) if a == b
    )
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

    const SPLIT_RECIPIENT: &str = "CNR1b172rotbSG6kCpfR76KB2ios2y7X4p8yEEc7pjLu";

    #[test]
    fn currency_symbols_normalize_but_mint_addresses_remain_case_sensitive() {
        let usdc = pay_types::Stablecoin::Usdc.mint(Some("mainnet"));
        assert!(same_currency("usdc", usdc, "mainnet"));
        assert!(same_currency(usdc, usdc, "mainnet"));
        let mut modified = usdc.as_bytes().to_vec();
        let index = modified
            .iter()
            .position(u8::is_ascii_alphabetic)
            .expect("USDC mint has letters");
        modified[index] = if modified[index].is_ascii_uppercase() {
            modified[index].to_ascii_lowercase()
        } else {
            modified[index].to_ascii_uppercase()
        };
        let other = String::from_utf8(modified).unwrap();
        assert!(!same_currency(&other, usdc, "mainnet"));
        assert!(!same_currency("USDC", &other, "mainnet"));
    }

    #[derive(Clone)]
    struct PathTestState;

    impl PaymentState for PathTestState {
        fn apis(&self) -> &[pay_types::metering::ApiSpec] {
            &[]
        }

        fn mpp(&self) -> Option<&pay_kit::mpp::server::Mpp> {
            None
        }
    }

    #[tokio::test]
    async fn axum_rejects_ambiguous_paths_without_calling_upstream() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tower::ServiceExt;

        let calls = Arc::new(AtomicUsize::new(0));
        let upstream_calls = calls.clone();
        let app = axum::Router::new()
            .fallback(move |uri: axum::http::Uri| {
                upstream_calls.fetch_add(1, Ordering::SeqCst);
                async move { uri.to_string() }
            })
            .layer(axum::middleware::from_fn_with_state(
                PathTestState,
                payment_middleware::<PathTestState>,
            ));
        for path in ["//foo", "///foo?x=1", "//.well-known/test"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        for path in ["/foo", "/foo?x=%2F&x=2", "/foo//bar?x=1"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap(),
                path
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn caller_cannot_forward_internal_policy_credentials() {
        let mut headers = HeaderMap::new();
        let internal = [
            "x-pay-gateway-host",
            "x-pay-wallet-resolver-proof",
            "x-pay-payment-policy",
            "x-pay-payment-policy-version",
        ];
        for name in internal {
            headers.append(name, HeaderValue::from_static("attacker"));
            headers.append(name, HeaderValue::from_static("second-value"));
            assert!(crate::server::proxy::STRIP_HEADERS.contains(&name));
        }
        headers.insert("accept", HeaderValue::from_static("application/json"));

        strip_internal_identity_headers(&mut headers);

        for name in internal {
            assert!(!headers.contains_key(name));
        }
        assert_eq!(headers["accept"], "application/json");
    }

    #[test]
    fn caller_cannot_spoof_internal_compute_identity() {
        let mut headers = HeaderMap::new();
        headers.insert("x-pay-verified-payer", HeaderValue::from_static("attacker"));
        headers.insert(
            "x-pay-verified-channel",
            HeaderValue::from_static("attacker-channel"),
        );
        headers.insert(
            "x-pay-original-host",
            HeaderValue::from_static("attacker.example"),
        );
        headers.insert("host", HeaderValue::from_static("function.compute.example"));

        strip_internal_identity_headers(&mut headers);

        assert!(!headers.contains_key("x-pay-verified-payer"));
        assert!(!headers.contains_key("x-pay-verified-channel"));
        assert!(!headers.contains_key("x-pay-original-host"));
        assert_eq!(headers.get("host").unwrap(), "function.compute.example");
    }

    #[test]
    fn verified_compute_identity_is_injected_after_spoofed_values_are_removed() {
        let mut headers = HeaderMap::new();
        headers.insert("x-pay-verified-payer", HeaderValue::from_static("attacker"));
        headers.insert(
            "x-pay-original-host",
            HeaderValue::from_static("attacker.example"),
        );

        strip_internal_identity_headers(&mut headers);
        inject_verified_payer_headers(
            &mut headers,
            "verified-payer",
            Some("hello.cpu.gcp.gateway-402.com"),
            Some("verified-channel"),
        );

        assert_eq!(
            headers.get("x-pay-verified-channel").unwrap(),
            "verified-channel"
        );
        assert_eq!(
            headers.get("x-pay-verified-payer").unwrap(),
            "verified-payer"
        );
        assert_eq!(
            headers.get("x-pay-original-host").unwrap(),
            "hello.cpu.gcp.gateway-402.com"
        );
    }

    #[test]
    fn original_host_is_injected_without_a_session_payer() {
        let mut headers = HeaderMap::new();
        inject_original_host_header(&mut headers, Some("hello.cpu.gcp.gateway-402.com"));
        assert_eq!(
            headers.get("x-pay-original-host").unwrap(),
            "hello.cpu.gcp.gateway-402.com"
        );
        assert!(!headers.contains_key("x-pay-verified-payer"));
    }

    fn configured_charge_splits() -> (pay_types::metering::ApiSpec, pay_types::metering::Metering) {
        let spec: pay_types::metering::ProviderSpec = serde_yml::from_str(&format!(
            r#"
provider: test
generated_at: "2026-09-02T00:00:00Z"
apis:
  - name: split-test
    subdomain: split-test
    title: "Split test"
    description: "Exercises configured MPP charge splits."
    category: ai_ml
    version: v1
    routing:
      type: respond
    recipients:
      rounding:
        account: "{SPLIT_RECIPIENT}"
        label: "Rounding leg"
      platform:
        account: "{SPLIT_RECIPIENT}"
        label: "Platform leg"
    endpoints:
      - method: POST
        path: v1/test
        metering:
          dimensions:
            - direction: usage
              unit: requests
              scale: 1
              tiers:
                - price_usd: 0.0015
          splits:
            # This rounds to zero USDC base units at six decimals.
            - recipient: rounding
              amount: 0.0000004
              memo: "Rounding adjustment"
            # A second leg to the same account is valid when its memo differs.
            - recipient: platform
              percent: 5
              memo: "Platform fee"
"#
        ))
        .expect("test YAML should parse");
        let api = spec.apis.into_iter().next().expect("test API should exist");
        let meter = api.endpoints[0]
            .metering
            .clone()
            .expect("test endpoint should be metered");
        (api, meter)
    }

    fn test_mpp() -> pay_kit::mpp::server::Mpp {
        pay_kit::mpp::server::Mpp::new(pay_kit::mpp::server::Config {
            recipient: SPLIT_RECIPIENT.to_string(),
            // An unreachable local RPC keeps challenge construction offline.
            rpc_url: Some("http://127.0.0.1:1".to_string()),
            challenge_binding_secret: Some(
                "test-challenge-binding-secret-must-be-32-bytes".to_string(),
            ),
            ..Default::default()
        })
        .expect("test MPP server should initialize")
    }

    fn challenge_splits_from_config() -> serde_json::Value {
        let (api, meter) = configured_charge_splits();
        let mpp = test_mpp();
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let splits = resolve_charge_splits(&mpp, &meter, &api, &uri, "0.0015");

        let challenge = mpp
            .charge_with_options(
                "0.0015",
                pay_kit::mpp::server::ChargeOptions {
                    splits,
                    ..Default::default()
                },
            )
            .expect("PayKit should accept resolved charge splits");
        let request: pay_kit::mpp::ChargeRequest = challenge
            .request
            .decode()
            .expect("PayKit challenge should decode");
        request
            .method_details
            .expect("PayKit charge challenge should include method details")
    }

    #[test]
    fn configured_charge_split_that_rounds_to_zero_reaches_pay_kit() {
        let details = challenge_splits_from_config();
        let splits = details["splits"]
            .as_array()
            .expect("PayKit challenge should include splits");

        assert_eq!(splits.len(), 2);
        assert_eq!(splits[0]["recipient"], SPLIT_RECIPIENT);
        assert_eq!(splits[0]["amount"], "0");
        assert_eq!(splits[0]["memo"], "Rounding adjustment");
    }

    #[test]
    fn configured_charge_splits_allow_same_recipient_with_distinct_memos() {
        let details = challenge_splits_from_config();
        let splits = details["splits"]
            .as_array()
            .expect("PayKit challenge should include splits");

        assert_eq!(splits[0]["recipient"], splits[1]["recipient"]);
        assert_eq!(splits[0]["memo"], "Rounding adjustment");
        assert_eq!(splits[1]["memo"], "Platform fee");
        assert_eq!(splits[1]["amount"], "75");
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

    #[tokio::test]
    async fn upto_unrelated_uploads_are_not_polled_or_capped_by_model_preparation() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        for (content_type, model_variant) in [
            (Some("application/octet-stream"), false),
            (Some("application/octet-stream"), true),
            (Some("application/json"), false),
            (None, true),
        ] {
            let mut plan = request_preparation_plan();
            if model_variant {
                plan.metering
                    .variants
                    .push(pay_types::metering::MeterVariant {
                        param: "model".into(),
                        value: "premium".into(),
                        description: None,
                        dimensions: plan.metering.dimensions.clone(),
                    });
            }
            let polled = Arc::new(AtomicBool::new(false));
            let observed = polled.clone();
            let length = MAX_METERED_MODEL_HINT_BODY_BYTES + 1;
            let body = Body::from_stream(futures_util::stream::once(async move {
                observed.store(true, Ordering::SeqCst);
                Ok::<_, std::convert::Infallible>(bytes::Bytes::from(vec![0xff; length]))
            }));
            let mut request = Request::builder()
                .uri("/v1/uploads")
                .header(header::CONTENT_LENGTH, length);
            if let Some(content_type) = content_type {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
            let request = request.body(body).unwrap();
            let (request, variant) = prepare_upto_request_body(request, "v1/uploads", &plan)
                .await
                .unwrap();
            assert!(
                !polled.load(Ordering::SeqCst),
                "{content_type:?} model_variant={model_variant}"
            );
            assert_eq!(variant, None);
            assert_eq!(
                request.headers()[header::CONTENT_LENGTH],
                length.to_string()
            );
            let bytes = axum::body::to_bytes(request.into_body(), length)
                .await
                .unwrap();
            assert_eq!(bytes.len(), length);
            assert!(bytes.iter().all(|byte| *byte == 0xff));
        }
    }

    fn request_preparation_plan() -> metering::UptoSettlementPlan {
        metering::UptoSettlementPlan {
            metering: serde_json::from_value(serde_json::json!({
                "dimensions": [{
                    "direction": "output", "unit": "bytes", "scale": 1,
                    "tiers": [{"price_usd": 0.000001}],
                    "meter": {"source": "response_header", "header": "x-usage-bytes"}
                }],
                "upto": {"max_usd": 1.0}
            }))
            .unwrap(),
            variant_hint: None,
            request_properties: RequestProperties::default(),
            ceiling_usd: 1.0,
            inferred_usage: None,
        }
    }

    #[tokio::test]
    async fn upto_prepares_only_chat_or_declared_json_model_variants() {
        let body = br#"{ "model": "premium", "stream": true }"#;
        let mut plan = request_preparation_plan();
        let request = || {
            Request::builder()
                .header(
                    header::CONTENT_TYPE,
                    "Application/Vnd.api+Json; charset=utf-8",
                )
                .body(Body::from(body.as_slice()))
                .unwrap()
        };
        // JSON with no model variants must not be rewritten, even with stream=true.
        let (untouched, model) = prepare_upto_request_body(request(), "v1/uploads", &plan)
            .await
            .unwrap();
        assert_eq!(model, None);
        assert_eq!(
            axum::body::to_bytes(untouched.into_body(), 1024)
                .await
                .unwrap()
                .as_ref(),
            body
        );

        plan.metering
            .variants
            .push(pay_types::metering::MeterVariant {
                param: "model".into(),
                value: "premium".into(),
                description: None,
                dimensions: plan.metering.dimensions.clone(),
            });
        let (restored, model) = prepare_upto_request_body(request(), "v1/responses", &plan)
            .await
            .unwrap();
        assert_eq!(model.as_deref(), Some("premium"));
        assert_eq!(
            axum::body::to_bytes(restored.into_body(), 1024)
                .await
                .unwrap()
                .as_ref(),
            body
        );

        // Chat must request usage even when the route already selected a model.
        plan.variant_hint = Some("path-model".into());
        let (restored, model) = prepare_upto_request_body(request(), "v1/chat/completions", &plan)
            .await
            .unwrap();
        assert_eq!(model.as_deref(), Some("path-model"));
        let body = axum::body::to_bytes(restored.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn metered_body_preserves_model_and_updates_length_for_stream_usage() {
        let original = br#"{"model":"premium","stream":true,"messages":[]}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, original.len())
            .body(Body::from(original.as_slice()))
            .unwrap();
        let (request, variant) = prepare_metered_request_body(request, true).await.unwrap();
        assert_eq!(variant.as_deref(), Some("premium"));
        assert_eq!(request.uri().path(), "/v1/chat/completions");
        let expected_length = request.headers()[header::CONTENT_LENGTH].clone();
        let body = axum::body::to_bytes(request.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(expected_length, body.len().to_string());
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "premium");
        assert_eq!(json["messages"], serde_json::json!([]));
        assert_eq!(json["stream_options"]["include_usage"], true);
    }

    #[test]
    fn delegated_json_body_reads_openai_compatible_model() {
        assert_eq!(
            prepare_metered_json_body(br#"{"model":"qwen3.7-max","stream":true}"#, false).0,
            Some("qwen3.7-max".to_string())
        );
    }

    #[test]
    fn delegated_json_body_rejects_missing_or_invalid_models() {
        assert_eq!(
            prepare_metered_json_body(br#"{"stream":true}"#, false).0,
            None
        );
        assert_eq!(
            prepare_metered_json_body(br#"{"model":"  "}"#, false).0,
            None
        );
        assert_eq!(prepare_metered_json_body(b"not json", false).0, None);
    }

    #[test]
    fn delegated_chat_stream_forces_provider_usage_frames() {
        let (variant, body) = prepare_metered_json_body(
            br#"{"model":"qwen3.7-plus","stream":true,"stream_options":{"include_usage":false}}"#,
            true,
        );
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(variant.as_deref(), Some("qwen3.7-plus"));
        assert_eq!(
            json.pointer("/stream_options/include_usage"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn delegated_non_stream_request_body_is_unchanged() {
        let body = br#"{"model":"qwen3.7-plus","stream":false}"#;
        let (_, prepared) = prepare_metered_json_body(body, true);
        assert_eq!(prepared, body);
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
    fn resource_memo_keeps_resource_and_adds_3_hex_char_nonce() {
        let memo = resource_memo_with_nonce(Some("fortune"), 566).unwrap();
        assert!(memo.starts_with("fortune#"));
        assert_eq!(memo.len(), "fortune#".len() + 3);
        assert!(resource_memo_matches(&memo, "fortune", 566));
    }

    #[test]
    fn resource_memo_matcher_accepts_legacy_static_resource() {
        assert!(resource_memo_matches("fortune", "fortune", 566));
        assert!(!resource_memo_matches("fortune#not-hex", "fortune", 566));
        assert!(!resource_memo_matches("other#012", "fortune", 566));
    }

    #[test]
    fn resource_memo_truncates_resource_to_keep_nonce_within_limit() {
        let resource = "fortune/very/long";
        let memo = resource_memo_with_nonce(Some(resource), 12).unwrap();
        assert!(memo.starts_with("f#t"));
        assert_eq!(memo.len(), 12);
        assert!(resource_memo_matches(&memo, resource, 12));
        assert!(!resource_memo_matches(
            &memo,
            resource,
            pay_kit::mpp::protocol::solana::MAX_MEMO_BYTES
        ));
    }

    #[test]
    fn resource_memo_matcher_rejects_short_resource_memo_for_longer_resource() {
        assert!(!resource_memo_matches("api/path#abc", "api/path/extra", 12));
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
