//! Request body preparation for metered forwards.
//!
//! A metered price may read the request body it forwards: its size bounds
//! input usage the provider does not report, and its model may select the
//! price. Both adapters prepare the body here so a request is priced the same
//! way whichever one serves it.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::server::metering;

const MAX_METERED_MODEL_HINT_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Only inference requests need body preparation; unrelated upto uploads keep
/// their original streaming bodies and limits.
/// Whether an x402 `upto` forward prices from its request body, and if so
/// whether to force stream usage reporting. Shared by both adapters so a
/// body-selected model is priced the same way whichever one serves it.
fn upto_body_preparation(
    path: &str,
    headers: &HeaderMap,
    plan: &metering::UptoSettlementPlan,
) -> Option<bool> {
    let chat = path == "chat/completions" || path.ends_with("/chat/completions");
    let json = headers
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
    (chat || needs_model).then_some(chat)
}

/// Whether a delegated session forward prices from its request body, and if
/// so whether to force stream usage reporting. Client-voucher sessions are
/// metered by the client's own commits and never read the body here.
pub(super) fn session_body_preparation(
    path: &str,
    forward: &crate::server::gate::SessionForward,
) -> Option<bool> {
    (forward.settlement.is_some()
        && forward
            .metered_plan()
            .is_some_and(|plan| plan.variant_hint.is_none()))
    .then(|| path.ends_with("chat/completions"))
}

/// Why a metered request body was refused before it could be priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeteredBodyRejection {
    TooLarge,
    PaddedModel,
    /// A compressed body hides its size and model from the meter, which
    /// would rate input by transport bytes the upstream then expands.
    EncodedBody,
}

impl MeteredBodyRejection {
    pub fn status(self) -> StatusCode {
        match self {
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::PaddedModel => StatusCode::BAD_REQUEST,
            Self::EncodedBody => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        }
    }

    pub fn body(self) -> &'static str {
        match self {
            Self::TooLarge => r#"{"error":"request_body_too_large"}"#,
            Self::PaddedModel => {
                r#"{"error":"invalid_model","message":"model must not have leading or trailing whitespace"}"#
            }
            Self::EncodedBody => {
                r#"{"error":"unsupported_content_encoding","message":"metered requests must be sent uncompressed"}"#
            }
        }
    }

    pub fn into_response(self) -> Response {
        Response::builder()
            .status(self.status())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(self.body()))
            .unwrap_or_else(|_| self.status().into_response())
    }
}

/// Price a buffered metered forward from the exact body it will send: select
/// the body's model variant for the session or `upto` plan, and record the
/// body size as the missing-input bound. Returns the bytes to forward when
/// the plan read the body (they may carry forced stream usage reporting), or
/// `None` when the body is forwarded untouched.
pub fn prepare_metered_forward_body(
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
    session: Option<&mut crate::server::gate::SessionForward>,
    upto: Option<&mut metering::UptoSettlementPlan>,
) -> Result<Option<Vec<u8>>, MeteredBodyRejection> {
    require_readable_body(headers, session.as_deref(), upto.as_deref())?;
    let mut prepared = None;
    if let Some(plan) = upto
        && let Some(force_stream_usage) = upto_body_preparation(path, headers, plan)
    {
        let (variant, bytes) = prepare_metered_body_bytes(body, force_stream_usage)?;
        plan.variant_hint = plan.variant_hint.take().or(variant);
        prepared = Some(bytes);
    }
    if let Some(forward) = session
        && let Some(force_stream_usage) = session_body_preparation(path, forward)
    {
        let source = prepared.as_deref().unwrap_or(body);
        let (variant, bytes) = prepare_metered_body_bytes(source, force_stream_usage)?;
        forward.request_body_bytes = Some(bytes.len() as u64);
        if let Some(plan) = forward.metered_plan_mut() {
            plan.variant_hint = variant;
        }
        prepared = Some(bytes);
    }
    Ok(prepared)
}

/// Whether pricing a forward reads its request body: a usage-metered price
/// bounds unreported input by the body's size and may select its price from
/// the body's model. Fixed and flat per-request prices never look at it.
fn prices_from_request_body(plan: &metering::UptoSettlementPlan) -> bool {
    metering::flat_request_price(&plan.metering).is_none()
}

/// Refuse a compressed body on a forward whose price reads it (see
/// [`require_identity_encoding`]); any other forward relays it as sent.
pub(super) fn require_readable_body(
    headers: &HeaderMap,
    session: Option<&crate::server::gate::SessionForward>,
    upto: Option<&metering::UptoSettlementPlan>,
) -> Result<(), MeteredBodyRejection> {
    let reads_body = session
        .and_then(crate::server::gate::SessionForward::metered_plan)
        .into_iter()
        .chain(upto)
        .any(prices_from_request_body);
    if reads_body {
        require_identity_encoding(headers)?;
    }
    Ok(())
}

/// Metered forwards rate the body they send, so it must be sent as is: the
/// size bounding unreported input usage and the model selecting the price
/// are only meaningful on uncompressed bytes.
pub fn require_identity_encoding(headers: &HeaderMap) -> Result<(), MeteredBodyRejection> {
    let identity = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .all(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .all(|coding| coding.is_empty() || coding.eq_ignore_ascii_case("identity"))
            })
        });
    if identity {
        Ok(())
    } else {
        Err(MeteredBodyRejection::EncodedBody)
    }
}

#[allow(clippy::result_large_err)]
pub(super) async fn prepare_upto_request_body(
    request: Request<Body>,
    path: &str,
    plan: &metering::UptoSettlementPlan,
) -> Result<(Request<Body>, Option<String>), Response> {
    require_readable_body(request.headers(), None, Some(plan))
        .map_err(MeteredBodyRejection::into_response)?;
    let Some(force_stream_usage) = upto_body_preparation(path, request.headers(), plan) else {
        return Ok((request, plan.variant_hint.clone()));
    };
    let (request, model) = prepare_metered_request_body(request, force_stream_usage).await?;
    Ok((request, plan.variant_hint.clone().or(model)))
}

// A direct `Response` error lets the middleware short-circuit without losing
// status, headers, or body content produced while reading the request body.
#[allow(clippy::result_large_err)]
pub(super) async fn prepare_metered_request_body(
    request: Request<Body>,
    force_stream_usage: bool,
) -> Result<(Request<Body>, Option<String>), Response> {
    let (mut parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_METERED_MODEL_HINT_BODY_BYTES)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to read metered request model");
            MeteredBodyRejection::TooLarge.into_response()
        })?;
    let (variant, bytes) = prepare_metered_body_bytes(&bytes, force_stream_usage)
        .map_err(MeteredBodyRejection::into_response)?;
    if force_stream_usage && let Ok(content_length) = bytes.len().to_string().parse() {
        parts.headers.insert(header::CONTENT_LENGTH, content_length);
    }
    Ok((Request::from_parts(parts, Body::from(bytes)), variant))
}

/// The model a metered body selects and the bytes to forward for it.
fn prepare_metered_body_bytes(
    body: &[u8],
    force_stream_usage: bool,
) -> Result<(Option<String>, Vec<u8>), MeteredBodyRejection> {
    if body.len() > MAX_METERED_MODEL_HINT_BODY_BYTES {
        return Err(MeteredBodyRejection::TooLarge);
    }
    if has_padded_model(body) {
        // Pricing, the upstream, and seller accounting must all see one model
        // identity; reject rather than normalize it for pricing only.
        return Err(MeteredBodyRejection::PaddedModel);
    }
    Ok(prepare_metered_json_body(body, force_stream_usage))
}

fn has_padded_model(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|json| json.get("model")?.as_str().map(|m| m.trim() != m))
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use http_body::Body as _;

    use crate::server::metering::RequestProperties;

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

    fn model_variant_plan() -> metering::UptoSettlementPlan {
        metering::UptoSettlementPlan {
            metering: serde_json::from_value(serde_json::json!({
                "dimensions": [{
                    "direction": "output",
                    "unit": "tokens",
                    "scale": 1,
                    "tiers": [{"price_usd": 0.000001}]
                }],
                "variants": [{
                    "param": "model",
                    "value": "premium",
                    "dimensions": [{
                        "direction": "output",
                        "unit": "tokens",
                        "scale": 1,
                        "tiers": [{"price_usd": 0.00001}]
                    }]
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

    fn session_forward(
        settlement: Option<crate::server::gate::SessionSettlementPlan>,
    ) -> crate::server::gate::SessionForward {
        let key = || solana_pubkey::Pubkey::new_unique().to_string();
        crate::server::gate::SessionForward {
            handle: std::sync::Arc::new(crate::server::session::SessionMpp::new(
                pay_kit::mpp::server::session::SessionConfig {
                    operator: key(),
                    recipient: key(),
                    currency: key(),
                    network: "localnet".to_string(),
                    ..Default::default()
                },
                "test-secret",
            )),
            channel_id: key(),
            committed_base_units: 0,
            settlement,
            available_base_units: 1_000_000,
            authorized_base_units: 1_000_000,
            verified_payer: None,
            request_body_bytes: None,
            payment_policy: None,
            _reservation: None,
            delegated: None,
        }
    }

    fn delegated_forward() -> crate::server::gate::SessionForward {
        session_forward(Some(crate::server::gate::SessionSettlementPlan::Metered(
            Box::new(model_variant_plan()),
        )))
    }

    #[test]
    fn buffered_delegated_session_prices_the_body_selected_model() {
        let body = br#"{"model":"premium","stream":true}"#;
        let mut forward = delegated_forward();

        let forwarded = prepare_metered_forward_body(
            "v1/chat/completions",
            &HeaderMap::new(),
            body,
            Some(&mut forward),
            None,
        )
        .unwrap()
        .expect("a body-variant session reads its body");

        let plan = forward.metered_plan().unwrap();
        assert_eq!(plan.variant_hint.as_deref(), Some("premium"));
        assert_eq!(forward.request_body_bytes, Some(forwarded.len() as u64));
        let json: serde_json::Value = serde_json::from_slice(&forwarded).unwrap();
        assert_eq!(json["stream_options"]["include_usage"], true);
    }

    #[test]
    fn buffered_metered_forwards_reject_a_padded_model() {
        let body = br#"{"model":" premium"}"#;
        let mut forward = delegated_forward();
        assert_eq!(
            prepare_metered_forward_body(
                "v1/chat/completions",
                &HeaderMap::new(),
                body,
                Some(&mut forward),
                None
            ),
            Err(MeteredBodyRejection::PaddedModel)
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let mut plan = model_variant_plan();
        assert_eq!(
            prepare_metered_forward_body("v1/generate", &headers, body, None, Some(&mut plan)),
            Err(MeteredBodyRejection::PaddedModel)
        );
    }

    #[test]
    fn metered_forwards_reject_compressed_bodies() {
        let body = br#"{"model":"premium"}"#;
        let encoded = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(value));
            headers
        };
        for coding in ["gzip", "identity, br", "GZIP"] {
            let mut forward = delegated_forward();
            assert_eq!(
                prepare_metered_forward_body(
                    "v1/chat/completions",
                    &encoded(coding),
                    body,
                    Some(&mut forward),
                    None,
                ),
                Err(MeteredBodyRejection::EncodedBody),
                "{coding}"
            );
            let mut plan = model_variant_plan();
            assert_eq!(
                prepare_metered_forward_body(
                    "v1/generate",
                    &encoded(coding),
                    body,
                    None,
                    Some(&mut plan)
                ),
                Err(MeteredBodyRejection::EncodedBody),
                "{coding}"
            );
        }

        // Positive controls: an explicit identity coding, and a client-voucher
        // session whose meter never reads the body.
        let mut forward = delegated_forward();
        assert!(
            prepare_metered_forward_body(
                "v1/chat/completions",
                &encoded("identity"),
                body,
                Some(&mut forward),
                None,
            )
            .unwrap()
            .is_some()
        );
        let mut client_voucher = session_forward(None);
        assert_eq!(
            prepare_metered_forward_body(
                "v1/chat/completions",
                &encoded("gzip"),
                body,
                Some(&mut client_voucher),
                None,
            ),
            Ok(None)
        );
    }

    #[test]
    fn compressed_bodies_pass_when_the_price_never_reads_them() {
        let body = br#"{"model":"premium"}"#;
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        let flat_plan = || metering::UptoSettlementPlan {
            metering: serde_json::from_value(serde_json::json!({
                "dimensions": [{
                    "direction": "usage",
                    "unit": "requests",
                    "scale": 1,
                    "tiers": [{"price_usd": 0.01}]
                }]
            }))
            .unwrap(),
            ..model_variant_plan()
        };

        // A fixed-price deployment session and a flat per-request price.
        let mut fixed = session_forward(Some(crate::server::gate::SessionSettlementPlan::Fixed {
            base_units: 1_000,
        }));
        assert_eq!(
            prepare_metered_forward_body("v1/upload", &headers, body, Some(&mut fixed), None),
            Ok(None)
        );
        let mut flat = session_forward(Some(crate::server::gate::SessionSettlementPlan::Metered(
            Box::new(flat_plan()),
        )));
        assert!(
            prepare_metered_forward_body("v1/upload", &headers, body, Some(&mut flat), None)
                .is_ok()
        );
        let mut plan = flat_plan();
        assert!(
            prepare_metered_forward_body("v1/upload", &headers, body, None, Some(&mut plan))
                .is_ok()
        );
        assert_eq!(
            require_readable_body(&headers, Some(&fixed), None),
            Ok(()),
            "the axum delegated check agrees"
        );
        // Positive control: a usage-metered price still reads the body.
        assert_eq!(
            require_readable_body(&headers, Some(&delegated_forward()), None),
            Err(MeteredBodyRejection::EncodedBody)
        );
    }

    #[tokio::test]
    async fn axum_upto_rejects_a_compressed_body() {
        let request = Request::builder()
            .header(header::CONTENT_ENCODING, "gzip")
            .body(Body::from("compressed"))
            .unwrap();
        let response =
            prepare_upto_request_body(request, "v1/chat/completions", &model_variant_plan())
                .await
                .unwrap_err();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn buffered_upto_prices_the_body_selected_model() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let mut plan = model_variant_plan();

        let forwarded = prepare_metered_forward_body(
            "v1/generate",
            &headers,
            br#"{"model":"premium"}"#,
            None,
            Some(&mut plan),
        )
        .unwrap();

        assert_eq!(
            forwarded.as_deref(),
            Some(br#"{"model":"premium"}"#.as_slice())
        );
        assert_eq!(plan.variant_hint.as_deref(), Some("premium"));
    }

    #[test]
    fn buffered_forwards_keep_their_body_when_pricing_does_not_read_it() {
        let body = br#"{"model":"premium","stream":true}"#;
        // Client-voucher sessions are metered by the client's own commits.
        let mut client_voucher = session_forward(None);
        assert_eq!(
            prepare_metered_forward_body(
                "v1/chat/completions",
                &HeaderMap::new(),
                body,
                Some(&mut client_voucher),
                None,
            ),
            Ok(None)
        );
        assert_eq!(client_voucher.request_body_bytes, None);

        // A path-selected variant is authoritative.
        let mut forward = delegated_forward();
        forward.metered_plan_mut().unwrap().variant_hint = Some("path-model".into());
        assert_eq!(
            prepare_metered_forward_body(
                "v1/chat/completions",
                &HeaderMap::new(),
                body,
                Some(&mut forward),
                None
            ),
            Ok(None)
        );
        assert_eq!(
            forward.metered_plan().unwrap().variant_hint.as_deref(),
            Some("path-model")
        );
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
    async fn metered_body_reports_its_forwarded_size_without_content_length() {
        // A chunked request carries no Content-Length; the buffered body the
        // gateway forwards is what bounds unreported input usage.
        let original = br#"{"model":"premium","stream":true,"messages":[{"role":"user","content":"long prompt"}]}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(futures_util::stream::iter([Ok::<
                _,
                std::io::Error,
            >(
                bytes::Bytes::from_static(original),
            )])))
            .unwrap();
        assert_eq!(request.body().size_hint().exact(), None);
        let (request, _) = prepare_metered_request_body(request, true).await.unwrap();
        let forwarded = request.body().size_hint().exact().unwrap();
        let body = axum::body::to_bytes(request.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(forwarded, body.len() as u64);
        assert!(
            forwarded > original.len() as u64,
            "stream usage was injected"
        );
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

    #[tokio::test]
    async fn metered_body_rejects_whitespace_padded_model() {
        let request = |body: &'static str| {
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        for padded in [
            r#"{"model":" fast ","messages":[]}"#,
            r#"{"model":"fast\n","messages":[]}"#,
            r#"{"model":"\tfast","messages":[]}"#,
        ] {
            let response = prepare_metered_request_body(request(padded), false)
                .await
                .expect_err("padded model must be rejected");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let (_, variant) = prepare_metered_request_body(request(r#"{"model":"fast"}"#), false)
            .await
            .unwrap();
        assert_eq!(variant.as_deref(), Some("fast"));
        let (_, variant) = prepare_metered_request_body(request(r#"{"messages":[]}"#), false)
            .await
            .unwrap();
        assert_eq!(variant, None);
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
}
