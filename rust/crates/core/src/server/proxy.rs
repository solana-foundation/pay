//! HTTP reverse proxy — forwards requests to upstream APIs.
//!
//! Resolves the upstream from `ApiSpec.routing`, forwards headers and body,
//! returns the upstream response. Strips hop-by-hop and payment headers.
//!
//! For `Respond` routing, returns 200 directly (no upstream call).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use base64::Engine;
use chrono::Utc;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use pay_types::metering::{
    AccessTokenFetchConfig, AccessTokenInjectConfig, AccessTokenResponseConfig, ApiSpec,
    AuthConfig, HmacAlgorithm, HmacCanonicalComponent, HmacCanonicalConfig, HmacDigestAlgorithm,
    HmacEncoding, HmacPrepareBinding, HmacPrepareValue, HmacQueryStyle, HmacSignatureConfig,
    HmacStringEncoding, HmacTargetType, HmacTimestampFormat, HttpMethod, RoutingConfig,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rand::RngCore;
use serde_json::json;
use sha1::Sha1;
use sha2::{Sha256, Sha512};
use tokio::sync::RwLock;
use uuid::Uuid;

pub use crate::server::gate::UpstreamRoute;
use crate::server::payment::TrustedPaymentIdentity;
use crate::server::session_stream::{self, SessionStreamContext};
use crate::server::{metering, payment, telemetry};

/// Headers to strip when forwarding to upstream. `pub` so the pingora data plane
/// (`pay-proxy`) shares the exact same list rather than duplicating it.
pub const STRIP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "transfer-encoding",
    "authorization",
    "payment-signature",
    "payment-required",
    // x402 v1 payment credential — must never reach a third-party upstream,
    // which could log or leak it (the gate accepts the credential via either
    // `PAYMENT-SIGNATURE` or `X-PAYMENT`).
    "x-payment",
    // Internal identity and routing metadata is minted only after payment
    // verification. Pingora forwards its native request header separately
    // from the sanitized HeaderMap, so these must be removed there too.
    "x-pay-verified-payer",
    "x-pay-verified-channel",
    "x-pay-original-host",
    "x-pay-gateway-host",
    "x-pay-wallet-resolver-proof",
    "x-pay-payment-policy",
    "x-pay-payment-policy-version",
    // Caller-supplied proxy routing metadata. The gateway admitted the request
    // for its own Host; a host-aware upstream must not be steered to another
    // tenant or resource under the gateway's upstream credential.
    "forwarded",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-forwarded-prefix",
    "x-forwarded-for",
    "x-real-ip",
];

/// Return an upstream URL that is safe to attach to logs and traces.
///
/// Query values may contain API keys, signed URLs, or user credentials, so the
/// observability boundary keeps parameter names for diagnosis but never their
/// values.
pub fn upstream_url_for_logging(url: &reqwest::Url) -> String {
    let mut safe = url.clone();
    if url.query().is_some() {
        let names = url
            .query_pairs()
            .map(|(name, _)| name.into_owned())
            .collect::<Vec<_>>();
        safe.set_query(None);
        for name in names {
            safe.query_pairs_mut().append_pair(&name, "[REDACTED]");
        }
    }
    safe.to_string()
}

/// Format a reqwest failure without allowing its attached URL to expose query
/// credentials in logs, traces, telemetry, or downstream error responses.
pub fn upstream_error_for_logging(error: &reqwest::Error) -> String {
    let mut safe = error.to_string();
    if let Some(url) = error.url() {
        safe = redact_url_in_error(&safe, url);
    }
    safe
}

/// HTTP client for every credential-bearing upstream and token request.
///
/// Redirects are never followed: a 3xx is returned to the caller as-is so
/// operator credentials and request bodies cannot reach another origin.
pub fn upstream_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client with default TLS config must build")
}

/// Remove query values from an arbitrary error string that may embed `url`.
pub fn redact_url_in_error(error: &str, url: &reqwest::Url) -> String {
    error.replace(url.as_str(), &upstream_url_for_logging(url))
}

/// Percent-encode every ASCII byte except RFC 3986 unreserved characters.
const RFC3986_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Resolve the effective routing for a request path.
///
/// This intentionally ignores the HTTP method. Metering/payment logic handles
/// method-sensitive gating separately, while routing overrides remain path-based
/// so browser payment-link and redirect flows can still inherit the endpoint's
/// transport behavior even when the browser uses `GET` against a non-GET
/// metered endpoint.
///
/// Endpoint selection mirrors the payment gate: literal paths win over
/// `{param}` / `{*rest}` patterns, so a concrete request the gate matched
/// against a pattern also inherits that endpoint's routing override.
///
/// Forwarding uses [`UpstreamRoute::routing`] instead, which follows the
/// endpoint the gate actually selected.
pub fn resolve_routing<'a>(api: &'a ApiSpec, path: &str) -> &'a RoutingConfig {
    metering::find_endpoint_by_path(api, path.trim_start_matches('/'))
        .and_then(|ep| ep.routing.as_ref())
        .unwrap_or(&api.routing)
}

/// Does the endpoint's upstream auth digest the **request body** (an HMAC /
/// AccessToken `prepare` binding with `body_digest`)? Such schemes can only be
/// signed correctly when the full body is available before the upstream connect.
///
/// The axum path always has the buffered body. The Pingora data plane uses this
/// signal to select its bounded request-buffering adapter instead of preparing a
/// signature against an empty placeholder body.
pub fn routing_signs_request_body(api: &ApiSpec, route: &UpstreamRoute) -> bool {
    use pay_types::metering::{AuthConfig, HmacPrepareValue};
    let prepares = match route.routing(api).auth() {
        Some(AuthConfig::Hmac { prepare, .. } | AuthConfig::AccessToken { prepare, .. }) => prepare,
        _ => return false,
    };
    prepares
        .iter()
        .any(|b| matches!(b.value, HmacPrepareValue::BodyDigest { .. }))
}

/// Forward a request to the upstream API defined in the spec.
///
/// - Builds the upstream URL from `api.routing` + request path
/// - Forwards all headers except hop-by-hop and payment headers
/// - Forwards the request body as-is
/// - Returns the upstream response (status, headers, body)
///
/// For `Respond` routing, returns 200 with `{"status":"ok"}`.
// A direct `Response` error preserves the proxy's intentionally constructed
// HTTP response for the axum caller.
#[allow(clippy::result_large_err)]
pub async fn forward_request(
    api: &ApiSpec,
    route: &UpstreamRoute,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    forward_request_with_session_metering(api, route, method, uri, headers, body, None).await
}

/// Forward a request and optionally meter the streamed response against an open
/// session. The plain [`forward_request`] wrapper keeps existing callers and
/// tests on the unmetered path.
#[tracing::instrument(
    name = "proxy_forward",
    skip(api, headers, body, session_context),
    fields(subdomain = %api.subdomain, method = %method, path = %uri.path())
)]
// A direct `Response` error preserves the proxy's intentionally constructed
// HTTP response for the axum caller.
#[allow(clippy::result_large_err)]
pub async fn forward_request_with_session_metering(
    api: &ApiSpec,
    route: &UpstreamRoute,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
    session_context: Option<SessionStreamContext>,
) -> Result<Response, Response> {
    forward_request_with_session_metering_and_identity(
        api,
        route,
        method,
        uri,
        headers,
        body,
        session_context,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
pub async fn forward_request_with_session_metering_and_identity(
    api: &ApiSpec,
    route: &UpstreamRoute,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
    session_context: Option<SessionStreamContext>,
    trusted_identity: Option<&TrustedPaymentIdentity>,
) -> Result<Response, Response> {
    let prepared = match prepare_upstream_with_identity(
        api,
        route,
        &method,
        uri,
        headers,
        body.as_ref(),
        trusted_identity,
    )
    .await?
    {
        UpstreamPlan::Respond(resp) => return Ok(resp),
        UpstreamPlan::Forward(prepared) => prepared,
    };

    let client = upstream_http_client();
    let mut upstream_req = client.request(
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap(),
        prepared.url.clone(),
    );
    for (name, value) in &prepared.headers {
        upstream_req = upstream_req.header(name.as_str(), value);
    }
    let upstream_url = prepared.url_for_logging();

    // Forward body. Always set content-length for POST/PUT/PATCH
    // (some upstreams like Google APIs require it even when empty).
    if !body.is_empty() {
        upstream_req = upstream_req.body(body.to_vec());
    } else if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
        upstream_req = upstream_req.header("content-length", "0");
    }

    let upstream_resp = upstream_req.send().await.map_err(|e| {
        let error = prepared.error_for_logging(&e);
        telemetry::record_upstream_error(&api.subdomain, uri.path(), &upstream_url, &error);
        tracing::error!(%error, upstream = %upstream_url, "Upstream request failed");
        error_response(StatusCode::BAD_GATEWAY, "Upstream request failed")
    })?;

    // Build response.
    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if status.is_server_error() {
        telemetry::record_upstream_error(
            &api.subdomain,
            uri.path(),
            &upstream_url,
            &format!("upstream returned {status}"),
        );
    }
    let mut response_headers = HeaderMap::new();
    // Skip headers that reqwest handles (it auto-decompresses gzip).
    let skip_response_headers = ["content-encoding", "content-length", "transfer-encoding"];
    for (name, value) in upstream_resp.headers() {
        let name_lower = name.as_str();
        if skip_response_headers.contains(&name_lower) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            response_headers.insert(n, v);
        }
    }
    prepared
        .url_credentials
        .scrub_response_headers(&mut response_headers);

    let mut resp = Response::builder().status(status);
    for (name, value) in &response_headers {
        resp = resp.header(name, value);
    }

    let content_type = response_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let is_sse = content_type
        .split(';')
        .any(|part| part.trim().eq_ignore_ascii_case("text/event-stream"));

    let meter = match session_context {
        Some(context) => {
            ClientVoucherMeter::for_route(api, route, uri, headers, context).map_err(|error| {
                // Releasing the response unmetered would hand out usage the
                // client's vouchers never cover.
                tracing::error!(%error, path = uri.path(), "session response metering unavailable");
                error_response(StatusCode::BAD_GATEWAY, "Session metering unavailable")
            })?
        }
        None => None,
    };
    let Some(meter) = meter.map(|meter| meter.with_request_body_bytes(body.len() as u64)) else {
        return Ok(resp
            .body(Body::from_stream(upstream_resp.bytes_stream()))
            .unwrap());
    };
    if crate::server::gate::is_streaming_response(&response_headers) {
        return Ok(resp
            .body(Body::from_stream(
                meter.meter_stream(upstream_resp.bytes_stream(), is_sse),
            ))
            .unwrap());
    }
    let body = collect_upstream_body(upstream_resp, meter.body_limit())
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to buffer session-metered response");
            error_response(StatusCode::BAD_GATEWAY, "Response metering failed")
        })?;
    if let Err(error) = meter.settle_body(&body).await {
        tracing::warn!(%error, "session-metered response usage not covered");
        return Err(session_voucher_required_response());
    }
    Ok(resp.body(Body::from(body)).unwrap())
}

/// Outcome of preparing an upstream request: either a locally-built response
/// (respond-mode / not-found) or a fully-prepared upstream request ready to be
/// sent (reqwest by axum) or proxied natively (pingora).
pub enum UpstreamPlan {
    Respond(Response),
    Forward(PreparedUpstreamRequest),
}

/// Resolve routing, build the upstream URL (with path rewrites), forward+strip
/// headers, and inject auth — everything up to (but not including) sending the
/// request. Framework-neutral: the result is consumed by axum's reqwest send and
/// by the pingora gate's native proxying alike.
// A direct `Response` error preserves status, headers, and body for both
// framework adapters without a lossy intermediate error type.
#[allow(clippy::result_large_err)]
pub async fn prepare_upstream(
    api: &ApiSpec,
    route: &UpstreamRoute,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<UpstreamPlan, Response> {
    prepare_upstream_with_identity(api, route, method, uri, headers, body, None).await
}

#[allow(clippy::result_large_err)]
pub async fn prepare_upstream_with_identity(
    api: &ApiSpec,
    route: &UpstreamRoute,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
    trusted_identity: Option<&TrustedPaymentIdentity>,
) -> Result<UpstreamPlan, Response> {
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());

    if crate::server::gate::has_dot_segment(uri.path()) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Invalid request path",
        ));
    }

    // Only the gate's admission of this exact request reaches the upstream.
    if !route.admits(api, method, uri.path()) {
        return Ok(UpstreamPlan::Respond(
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"error":"not_found"}"#))
                .unwrap(),
        ));
    }

    let routing = route.routing(api);

    // Respond mode — no upstream call.
    if routing.is_respond() {
        use crate::server::metering::find_endpoint_by_path;
        let path_trimmed = uri.path().trim_start_matches('/');
        if find_endpoint_by_path(api, path_trimmed).is_some() {
            return Ok(UpstreamPlan::Respond(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"status":"ok"}"#))
                    .unwrap(),
            ));
        }
        return Ok(UpstreamPlan::Respond(
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"error":"not_found"}"#))
                .unwrap(),
        ));
    }

    // Build the upstream URL (with path rewrites) and prepare the forwarded
    // request state before converting it into a reqwest request.
    let upstream_url = routing
        .upstream_url(path_and_query)
        .expect("Proxy routing must have a URL");
    let mut prepared = PreparedUpstreamRequest::new(&upstream_url).map_err(|e| {
        telemetry::record_upstream_error(&api.subdomain, uri.path(), "[invalid upstream URL]", &e);
        error_response(StatusCode::BAD_GATEWAY, &e)
    })?;
    if let RoutingConfig::Proxy { path_rewrites, .. } = routing {
        // Rewrite values come from the environment and may be credentials.
        for rewrite in path_rewrites {
            if let Ok(value) = std::env::var(&rewrite.env) {
                prepared.url_credentials.push_path(&value);
            }
        }
    }

    let upstream_log_url = prepared.url_for_logging();
    tracing::debug!(
        subdomain = %api.subdomain,
        upstream = %upstream_log_url,
        "Forwarding request"
    );

    // Forward headers.
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if STRIP_HEADERS.contains(&name_str) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            prepared.add_forwarded_header(name_str, v);
        }
    }

    if let Some(identity) = trusted_identity {
        let mut trusted_headers = HeaderMap::new();
        payment::inject_original_host_header(
            &mut trusted_headers,
            identity.original_host.as_deref(),
        );
        if let Some(payer) = identity.payer.as_deref() {
            payment::inject_verified_payer_headers(
                &mut trusted_headers,
                payer,
                None,
                identity.channel_id.as_deref(),
            );
        }
        payment::inject_payment_policy_header(
            &mut trusted_headers,
            identity.payment_policy.as_deref(),
        );
        for (name, value) in trusted_headers {
            if let Some(name) = name
                && let Ok(value) = value.to_str()
            {
                prepared.add_forwarded_header(name.as_str(), value);
            }
        }
    }

    // Inject auth into the prepared upstream request.
    match routing.auth() {
        Some(
            AuthConfig::Header { .. } | AuthConfig::QueryParam { .. } | AuthConfig::Hmac { .. },
        ) => {
            apply_prepared_request_auth(
                &mut prepared,
                method,
                body,
                routing.auth().expect("routing auth checked above"),
            )
            .map_err(|e| {
                telemetry::record_upstream_error(
                    &api.subdomain,
                    uri.path(),
                    &prepared.url_for_logging(),
                    &e,
                );
                error_response(StatusCode::BAD_GATEWAY, &e)
            })?;
        }
        Some(AuthConfig::Oauth2 {
            token_url,
            scopes,
            audience,
            client_id_from_env,
            client_secret_from_env,
            headers,
        }) => {
            match oauth2_token(
                token_url,
                scopes,
                audience.as_deref(),
                client_id_from_env.as_deref(),
                client_secret_from_env.as_deref(),
            )
            .await
            {
                Ok(token) => {
                    prepared.set_header("authorization", format!("Bearer {token}"));
                    for (header_name, env_ref) in headers {
                        if let Ok(val) = std::env::var(&env_ref.from_env) {
                            prepared.set_header(header_name, val);
                        }
                    }
                }
                Err(e) => {
                    telemetry::record_upstream_error(
                        &api.subdomain,
                        uri.path(),
                        &prepared.url_for_logging(),
                        &format!("OAuth2 token error: {e}"),
                    );
                    tracing::error!(error = %e, "Failed to fetch OAuth2 token");
                    return Err(error_response(
                        StatusCode::BAD_GATEWAY,
                        "OAuth2 token request failed",
                    ));
                }
            }
        }
        Some(AuthConfig::AccessToken {
            prepare,
            fetch,
            inject,
        }) => {
            apply_access_token_auth(&mut prepared, method, body, prepare, fetch, inject)
                .await
                .map_err(|e| {
                    telemetry::record_upstream_error(
                        &api.subdomain,
                        uri.path(),
                        &prepared.url_for_logging(),
                        &e,
                    );
                    tracing::error!(error = %e, "Failed to fetch access token");
                    error_response(StatusCode::BAD_GATEWAY, "Access token request failed")
                })?;
        }
        _ => {}
    }

    Ok(UpstreamPlan::Forward(prepared))
}

/// Response meter for a client-voucher session forward: usage is released
/// only once the client's vouchers cover it. Shared by both adapters and
/// keyed to the endpoint the gate billed.
pub struct ClientVoucherMeter {
    meter: session_stream::SessionStreamMeter,
    body_limit: usize,
}

impl ClientVoucherMeter {
    /// `Ok(None)` means the endpoint's price is not observable in the
    /// response (request-only pricing, already covered by the admitting
    /// voucher).
    pub fn for_route(
        api: &ApiSpec,
        route: &UpstreamRoute,
        uri: &Uri,
        headers: &HeaderMap,
        context: SessionStreamContext,
    ) -> crate::server::session_metering::Result<Option<Self>> {
        let Some(metering) = route.endpoint(api).and_then(|ep| ep.metering.as_ref()) else {
            return Ok(None);
        };
        let path = uri.path().trim_start_matches('/');
        let props = payment::extract_request_properties(headers, path);
        let variant_hint = payment::extract_variant_hint(path);
        let meter =
            session_stream::meter_from_config(metering, &props, variant_hint.as_deref(), context)?;
        Ok(meter.map(|meter| Self {
            meter,
            body_limit: metering::upto_response_body_limit(metering),
        }))
    }

    /// Use the exact forwarded request body size as the missing-input bound.
    pub fn with_request_body_bytes(mut self, bytes: u64) -> Self {
        self.meter = self.meter.with_request_body_bytes(bytes);
        self
    }

    /// Largest complete response body this endpoint meters.
    pub fn body_limit(&self) -> usize {
        self.body_limit
    }

    /// Meter a streamed response: each chunk is withheld until a commit
    /// covers the usage it carries.
    pub fn meter_stream<S>(
        self,
        stream: S,
        is_sse: bool,
    ) -> impl futures_util::Stream<Item = Result<Bytes, session_stream::BoxError>> + Send + 'static
    where
        S: futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    {
        session_stream::meter_response_stream(stream, self.meter, is_sse)
    }

    /// Meter a complete response body, returning once the client's vouchers
    /// cover it. Its usage is known only after the whole body is read.
    pub async fn settle_body(self, body: &[u8]) -> Result<(), session_stream::BoxError> {
        self.meter.settle_complete_body(body, false).await
    }
}

/// Read a complete upstream response body, refusing one larger than `limit`.
pub async fn collect_upstream_body(
    response: reqwest::Response,
    limit: usize,
) -> Result<Bytes, session_stream::BoxError> {
    use futures_util::StreamExt;
    let too_large = || -> session_stream::BoxError {
        Box::new(std::io::Error::other(
            "response body exceeds configured limit",
        ))
    };
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large());
    }
    let mut stream = response.bytes_stream();
    let mut buf = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buf.len().saturating_add(chunk.len()) > limit {
            return Err(too_large());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// The response for a client-voucher session whose usage no voucher covered.
pub fn session_voucher_required_response() -> Response {
    error_response(
        StatusCode::PAYMENT_REQUIRED,
        "Commit a session voucher covering this response",
    )
}

/// Resolve the API spec from a Host header subdomain.
pub fn resolve_api<'a>(apis: &'a [ApiSpec], host: &str) -> Option<&'a ApiSpec> {
    let subdomain = host.split('.').next().unwrap_or("");
    apis.iter().find(|a| a.subdomain == subdomain)
}

pub fn error_response(status: StatusCode, message: &str) -> Response {
    let body = json!({
        "error": status.as_str(),
        "message": message,
    });

    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedUpstreamRequest {
    pub url: reqwest::Url,
    pub headers: Vec<(String, String)>,
    /// Secrets the gateway placed in the upstream URL. They must never come
    /// back to the caller (e.g. in a relayed redirect) or reach a log.
    pub url_credentials: UrlCredentials,
}

/// Secrets the gateway placed in an upstream URL: query credentials (API
/// keys, HMAC signatures, fetched access tokens) and environment-backed path
/// rewrite values. Kept to scrub them from responses and diagnostics.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct UrlCredentials {
    /// Whole query values: redacted wherever they appear.
    query: Vec<String>,
    /// Path segments a rewrite inserted: redacted where they form a segment,
    /// since a short identifier can also occur as unrelated text.
    path_segments: Vec<String>,
}

/// Response headers that carry URLs the caller follows or reads.
pub const URL_RESPONSE_HEADERS: [axum::http::HeaderName; 2] = [
    axum::http::header::LOCATION,
    axum::http::header::CONTENT_LOCATION,
];

impl UrlCredentials {
    fn push(&mut self, value: &str) {
        if !value.is_empty() {
            self.query.push(value.to_string());
        }
    }

    /// Record a value a path rewrite inserted into the upstream path.
    fn push_path(&mut self, value: &str) {
        self.path_segments.extend(
            value
                .split('/')
                .filter(|segment| !segment.is_empty())
                .map(str::to_string),
        );
    }

    /// `url` safe for logs, traces and telemetry: query values and any path
    /// segment carrying a secret are redacted.
    pub fn url_for_logging(&self, url: &reqwest::Url) -> String {
        let mut safe = upstream_url_for_logging(url);
        if let Ok(mut parsed) = reqwest::Url::parse(&safe)
            && let Some(segments) = parsed.path_segments().map(|segments| {
                segments
                    .map(|segment| {
                        let decoded = percent_encoding::percent_decode_str(segment)
                            .decode_utf8_lossy()
                            .into_owned();
                        if self.path_segments.contains(&decoded)
                            || self
                                .query
                                .iter()
                                .any(|secret| decoded.contains(secret.as_str()))
                        {
                            "[REDACTED]".to_string()
                        } else {
                            decoded
                        }
                    })
                    .collect::<Vec<_>>()
            })
            && parsed
                .path_segments_mut()
                .map(|mut path| {
                    path.clear().extend(segments);
                })
                .is_ok()
        {
            safe = parsed.to_string();
        }
        self.redact(&safe)
    }

    /// A reqwest failure safe for logs: its attached URL is redacted.
    pub fn error_for_logging(&self, error: &reqwest::Error) -> String {
        let mut safe = error.to_string();
        if let Some(url) = error.url() {
            safe = safe.replace(url.as_str(), &self.url_for_logging(url));
        }
        self.redact(&safe)
    }

    /// `text` with every secret, raw or percent-encoded, replaced.
    pub fn redact(&self, text: &str) -> String {
        let mut safe = text.to_string();
        for secret in &self.query {
            for form in Self::forms(secret) {
                safe = safe.replace(&form, "[REDACTED]");
            }
        }
        safe
    }

    fn forms(secret: &str) -> [String; 3] {
        [
            secret.to_string(),
            url::form_urlencoded::byte_serialize(secret.as_bytes()).collect(),
            percent_encoding::utf8_percent_encode(secret, percent_encoding::NON_ALPHANUMERIC)
                .to_string(),
        ]
    }

    fn exposed_in(&self, value: &str) -> bool {
        self.query.iter().any(|secret| {
            Self::forms(secret)
                .iter()
                .any(|form| value.contains(form.as_str()))
                || percent_encoding::percent_decode_str(value)
                    .decode_utf8_lossy()
                    .contains(secret.as_str())
        })
    }

    /// `value` with every query pair carrying a credential removed, or
    /// `None` when the credential cannot be removed from it.
    fn scrub_url(&self, value: &str) -> Option<String> {
        if !self.exposed_in(value) {
            return Some(value.to_string());
        }
        let base = reqwest::Url::parse("http://relative.invalid/").ok()?;
        let absolute = reqwest::Url::parse(value).is_ok();
        let mut url = base.join(value).ok()?;
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(_, pair)| !self.query.iter().any(|secret| secret == pair))
            .map(|(key, pair)| (key.into_owned(), pair.into_owned()))
            .collect();
        url.set_query(None);
        if !kept.is_empty() {
            url.query_pairs_mut().extend_pairs(kept);
        }
        let scrubbed = if absolute {
            url.to_string()
        } else {
            url[url::Position::BeforePath..].to_string()
        };
        (!self.exposed_in(&scrubbed)).then_some(scrubbed)
    }

    /// Remove injected credentials from URL-bearing response headers; a
    /// header that would still expose one is dropped.
    pub fn scrub_response_headers(&self, headers: &mut HeaderMap) {
        if self.query.is_empty() {
            return;
        }
        for name in URL_RESPONSE_HEADERS {
            let values: Vec<HeaderValue> = headers.get_all(&name).iter().cloned().collect();
            if values.is_empty() {
                continue;
            }
            headers.remove(&name);
            for value in values {
                if let Some(scrubbed) = value
                    .to_str()
                    .ok()
                    .and_then(|value| self.scrub_url(value))
                    .and_then(|value| HeaderValue::from_str(&value).ok())
                {
                    headers.append(name.clone(), scrubbed);
                }
            }
        }
    }
}

impl std::fmt::Debug for PreparedUpstreamRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_names = self
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        formatter
            .debug_struct("PreparedUpstreamRequest")
            .field("url", &self.url_for_logging())
            .field("header_names", &header_names)
            .finish()
    }
}

impl PreparedUpstreamRequest {
    /// Parse the routed upstream URL into a mutable request shape that can be
    /// amended by auth code before we build the final `reqwest` request.
    fn new(upstream_url: &str) -> Result<Self, String> {
        let url =
            reqwest::Url::parse(upstream_url).map_err(|e| format!("Invalid upstream URL: {e}"))?;
        Ok(Self {
            url,
            headers: Vec::new(),
            url_credentials: UrlCredentials::default(),
        })
    }

    /// The upstream URL safe for logs, traces and telemetry.
    pub fn url_for_logging(&self) -> String {
        self.url_credentials.url_for_logging(&self.url)
    }

    /// A reqwest failure for this request, safe for logs.
    pub fn error_for_logging(&self, error: &reqwest::Error) -> String {
        self.url_credentials.error_for_logging(error)
    }

    /// Free text that may quote this request's URL, safe for logs.
    pub fn redact_for_logging(&self, text: &str) -> String {
        self.url_credentials
            .redact(&text.replace(self.url.as_str(), &self.url_for_logging()))
    }

    fn add_forwarded_header(&mut self, name: &str, value: &str) {
        self.headers.push((name.to_string(), value.to_string()));
    }

    fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.into()));
    }

    fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn append_query_credential(&mut self, name: &str, value: &str) {
        self.url.query_pairs_mut().append_pair(name, value);
        self.url_credentials.push(value);
    }

    fn set_query_param(&mut self, name: &str, value: &str) {
        let existing: Vec<(String, String)> = self
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .filter(|(key, _)| key != name)
            .collect();
        self.url.set_query(None);
        {
            let mut pairs = self.url.query_pairs_mut();
            for (key, existing_value) in existing {
                pairs.append_pair(&key, &existing_value);
            }
            pairs.append_pair(name, value);
        }
    }
}

/// Apply a direct auth scheme to a mutable upstream request.
///
/// This helper is intentionally limited to schemes that operate entirely on the
/// already-prepared request. Token-fetching flows are handled separately.
struct HmacAuthConfigRef<'a> {
    algorithm: &'a HmacAlgorithm,
    secret_from_env: &'a str,
    secret_suffix: Option<&'a str>,
    key_id_from_env: Option<&'a str>,
    prepare: &'a [HmacPrepareBinding],
    canonical: &'a HmacCanonicalConfig,
    signature: &'a HmacSignatureConfig,
}

fn apply_prepared_request_auth(
    prepared: &mut PreparedUpstreamRequest,
    method: &Method,
    body: &[u8],
    auth: &AuthConfig,
) -> Result<(), String> {
    match auth {
        AuthConfig::Header {
            key,
            prefix,
            value_from_env,
        } => {
            // Same loud-failure rule as QueryParam — silently sending an
            // empty header value masks the real config problem behind a
            // confusing upstream 401/403.
            let secret = std::env::var(value_from_env).map_err(|_| {
                format!(
                    "Proxy auth misconfigured: env var `{value_from_env}` is not set, \
                     can't inject the `{key}` header into the upstream request. \
                     Export it in the shell that runs `pay gate api`."
                )
            })?;
            if secret.trim().is_empty() {
                return Err(format!(
                    "Proxy auth misconfigured: env var `{value_from_env}` is set but empty, \
                     can't inject the `{key}` header into the upstream request."
                ));
            }
            let value = match prefix {
                Some(p) => format!("{p}{secret}"),
                None => secret,
            };
            prepared.set_header(key, value);
            Ok(())
        }
        AuthConfig::QueryParam {
            key,
            value_from_env,
        } => {
            // Silently appending `?{key}=` (empty) when the env var is
            // unset trips upstream auth checks with cryptic errors like
            // the provider's `"missing api key"`. Fail loudly so operators
            // notice the missing config at the first request instead of
            // chasing it through upstream logs.
            let secret = std::env::var(value_from_env).map_err(|_| {
                format!(
                    "Proxy auth misconfigured: env var `{value_from_env}` is not set, \
                     can't inject `?{key}=...` into the upstream request. \
                     Export it in the shell that runs `pay gate api`."
                )
            })?;
            if secret.trim().is_empty() {
                return Err(format!(
                    "Proxy auth misconfigured: env var `{value_from_env}` is set but empty, \
                     can't inject `?{key}=...` into the upstream request."
                ));
            }
            prepared.append_query_credential(key, &secret);
            Ok(())
        }
        AuthConfig::Hmac {
            algorithm,
            secret_from_env,
            secret_suffix,
            key_id_from_env,
            prepare,
            canonical,
            signature,
        } => apply_hmac_auth(
            prepared,
            method,
            body,
            &HmacAuthConfigRef {
                algorithm,
                secret_from_env,
                secret_suffix: secret_suffix.as_deref(),
                key_id_from_env: key_id_from_env.as_deref(),
                prepare,
                canonical,
                signature,
            },
        ),
        AuthConfig::Oauth2 { .. } => Err(
            "Nested oauth2 auth is not supported when preparing another upstream request"
                .to_string(),
        ),
        AuthConfig::AccessToken { .. } => Err(
            "Nested access_token auth is not supported when preparing another upstream request"
                .to_string(),
        ),
    }
}

/// Apply a cached access-token flow to the main upstream request.
async fn apply_access_token_auth(
    prepared: &mut PreparedUpstreamRequest,
    _method: &Method,
    body: &[u8],
    prepare: &[HmacPrepareBinding],
    fetch: &AccessTokenFetchConfig,
    inject: &AccessTokenInjectConfig,
) -> Result<(), String> {
    for binding in prepare {
        let value = resolve_hmac_prepare_value(&prepared.url, body, &binding.value)?;
        apply_hmac_target(prepared, &binding.target.kind, &binding.target.name, &value);
    }

    let token = access_token(fetch).await?;
    let rendered = render_access_token_template(&inject.template, &token)?;
    apply_credential_target(
        prepared,
        &inject.target.kind,
        &inject.target.name,
        &rendered,
    );
    Ok(())
}

/// Apply generic HMAC request signing to the prepared upstream request.
///
/// The sequence is:
/// 1. resolve the configured secret/key id from the environment
/// 2. apply all `prepare` bindings to the mutable request
/// 3. build the canonical string from the post-prepare request state
/// 4. compute the HMAC and write the rendered signature to its destination
fn apply_hmac_auth(
    prepared: &mut PreparedUpstreamRequest,
    method: &Method,
    body: &[u8],
    auth: &HmacAuthConfigRef<'_>,
) -> Result<(), String> {
    let secret = std::env::var(auth.secret_from_env)
        .map_err(|_| format!("HMAC secret env var not set: {}", auth.secret_from_env))?;
    let key_id = match auth.key_id_from_env {
        Some(env_name) => Some(
            std::env::var(env_name)
                .map_err(|_| format!("HMAC key ID env var not set: {env_name}"))?,
        ),
        None => None,
    };

    for binding in auth.prepare {
        let value = resolve_hmac_prepare_value(&prepared.url, body, &binding.value)?;
        apply_hmac_target(prepared, &binding.target.kind, &binding.target.name, &value);
    }

    let canonical_string = build_hmac_canonical_string(prepared, method, auth.canonical)?;
    let signing_secret = match auth.secret_suffix {
        Some(suffix) => format!("{secret}{suffix}"),
        None => secret,
    };
    let signature_bytes = compute_hmac_signature_bytes(
        auth.algorithm,
        signing_secret.as_bytes(),
        canonical_string.as_bytes(),
    )?;
    let encoded_signature = encode_bytes(signature_bytes.as_ref(), &auth.signature.encoding);
    let rendered_signature = render_hmac_template(
        &auth.signature.destination.template,
        encoded_signature.as_str(),
        key_id.as_deref(),
    )?;

    apply_credential_target(
        prepared,
        &auth.signature.destination.kind,
        &auth.signature.destination.name,
        &rendered_signature,
    );
    Ok(())
}

/// Write a resolved value to either a header or query parameter.
fn apply_hmac_target(
    prepared: &mut PreparedUpstreamRequest,
    kind: &HmacTargetType,
    name: &str,
    value: &str,
) {
    match kind {
        HmacTargetType::Header => prepared.set_header(name, value),
        HmacTargetType::QueryParam => prepared.set_query_param(name, value),
    }
}

/// Write a credential (signature, access token) to its target, remembering a
/// query credential so it can be scrubbed from the response.
fn apply_credential_target(
    prepared: &mut PreparedUpstreamRequest,
    kind: &HmacTargetType,
    name: &str,
    value: &str,
) {
    apply_hmac_target(prepared, kind, name, value);
    if matches!(kind, HmacTargetType::QueryParam) {
        prepared.url_credentials.push(value);
    }
}

/// Resolve one `prepare` binding against the final upstream URL and raw body.
fn resolve_hmac_prepare_value(
    upstream_url: &reqwest::Url,
    body: &[u8],
    value: &HmacPrepareValue,
) -> Result<String, String> {
    match value {
        HmacPrepareValue::Literal { value } => Ok(value.clone()),
        HmacPrepareValue::Env { from_env } => {
            std::env::var(from_env).map_err(|_| format!("HMAC prepare env var not set: {from_env}"))
        }
        HmacPrepareValue::UpstreamHost {} => host_header_value(upstream_url),
        HmacPrepareValue::Timestamp { format } => Ok(match format {
            HmacTimestampFormat::Rfc1123Gmt => current_gmt_date(),
            HmacTimestampFormat::Iso8601Zulu => current_iso8601_zulu(),
            HmacTimestampFormat::UnixSeconds => Utc::now().timestamp().to_string(),
        }),
        HmacPrepareValue::UuidV4 {} => Ok(random_uuid_v4()),
        HmacPrepareValue::RandomHex { bytes } => Ok(random_hex(*bytes as usize)),
        HmacPrepareValue::BodyDigest {
            algorithm,
            encoding,
        } => Ok(encode_bytes(
            digest_bytes(algorithm, body).as_ref(),
            encoding,
        )),
    }
}

/// Render the canonical string that becomes the HMAC message.
///
/// Header lookups are case-insensitive and operate on the post-prepare header
/// set. Query lookups operate on the final upstream URL after any prepare-time
/// query mutations.
fn build_hmac_canonical_string(
    prepared: &PreparedUpstreamRequest,
    method: &Method,
    canonical: &HmacCanonicalConfig,
) -> Result<String, String> {
    let mut parts = Vec::with_capacity(canonical.components.len());

    for component in &canonical.components {
        let part = match component {
            HmacCanonicalComponent::Method {} => method.as_str().to_string(),
            HmacCanonicalComponent::Path {} => prepared.url.path().to_string(),
            HmacCanonicalComponent::Query { style, encoding } => {
                canonical_query_component(&prepared.url, style, encoding)?
            }
            HmacCanonicalComponent::Header { name } => prepared
                .header_value(name)
                .map(str::to_string)
                .ok_or_else(|| format!("HMAC canonical header `{name}` is missing"))?,
            HmacCanonicalComponent::Headers {
                names,
                join_with,
                format,
            } => {
                let mut rendered = Vec::with_capacity(names.len());
                for name in names {
                    let value = prepared
                        .header_value(name)
                        .ok_or_else(|| format!("HMAC canonical header `{name}` is missing"))?;
                    rendered.push(render_named_value_template(format, name, value)?);
                }
                rendered.join(join_with)
            }
            HmacCanonicalComponent::Literal { value } => value.clone(),
        };
        parts.push(part);
    }

    Ok(parts.join(&canonical.join_with))
}

/// Render the final query string for canonicalization.
///
/// `raw` preserves the exact query string order and encoding already present on
/// the upstream URL. `sorted_pairs` reorders pairs by name and then value using
/// the raw `k=v` substrings.
fn canonical_query_component(
    url: &reqwest::Url,
    style: &HmacQueryStyle,
    encoding: &HmacStringEncoding,
) -> Result<String, String> {
    let raw_query = url
        .query()
        .filter(|query| !query.is_empty())
        .ok_or_else(|| "HMAC canonical query is missing".to_string())?;
    let rendered = match style {
        HmacQueryStyle::Raw => raw_query.to_string(),
        HmacQueryStyle::SortedPairs => {
            let mut pairs = parse_raw_query_pairs(raw_query);
            pairs.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
            pairs
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("&")
        }
    };
    Ok(match encoding {
        HmacStringEncoding::None => rendered,
        HmacStringEncoding::PercentRfc3986 => percent_encode_rfc3986(&rendered),
    })
}

/// Split a raw query string into `name=value` pairs without decoding it.
fn parse_raw_query_pairs(raw_query: &str) -> Vec<(String, String)> {
    raw_query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (name.to_string(), value.to_string()),
            None => (pair.to_string(), String::new()),
        })
        .collect()
}

/// Compute the raw HMAC bytes for the configured hash algorithm.
fn compute_hmac_signature_bytes(
    algorithm: &HmacAlgorithm,
    secret: &[u8],
    message: &[u8],
) -> Result<Vec<u8>, String> {
    match algorithm {
        HmacAlgorithm::Sha1 => {
            let mut mac = Hmac::<Sha1>::new_from_slice(secret)
                .map_err(|e| format!("Invalid HMAC secret: {e}"))?;
            mac.update(message);
            Ok(mac.finalize().into_bytes().to_vec())
        }
        HmacAlgorithm::Sha256 => {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret)
                .map_err(|e| format!("Invalid HMAC secret: {e}"))?;
            mac.update(message);
            Ok(mac.finalize().into_bytes().to_vec())
        }
        HmacAlgorithm::Sha512 => {
            let mut mac = Hmac::<Sha512>::new_from_slice(secret)
                .map_err(|e| format!("Invalid HMAC secret: {e}"))?;
            mac.update(message);
            Ok(mac.finalize().into_bytes().to_vec())
        }
    }
}

/// Compute a raw body digest for `prepare.value.from: body_digest`.
fn digest_bytes(algorithm: &HmacDigestAlgorithm, body: &[u8]) -> Vec<u8> {
    match algorithm {
        HmacDigestAlgorithm::Md5 => {
            let mut hasher = Md5::new();
            hasher.update(body);
            hasher.finalize().to_vec()
        }
        HmacDigestAlgorithm::Sha256 => {
            let mut hasher = Sha256::new();
            hasher.update(body);
            hasher.finalize().to_vec()
        }
        HmacDigestAlgorithm::Sha512 => {
            let mut hasher = Sha512::new();
            hasher.update(body);
            hasher.finalize().to_vec()
        }
    }
}

/// Encode raw bytes as either base64 or lowercase hex.
fn encode_bytes(bytes: &[u8], encoding: &HmacEncoding) -> String {
    match encoding {
        HmacEncoding::Base64 => base64::engine::general_purpose::STANDARD.encode(bytes),
        HmacEncoding::Hex => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
    }
}

/// Render the configured signature destination template.
fn render_hmac_template(
    template: &str,
    signature: &str,
    key_id: Option<&str>,
) -> Result<String, String> {
    render_template(template, |token| match token {
        "signature" => Some(signature.to_string()),
        "key_id" => key_id.map(str::to_string),
        _ => None,
    })
}

/// Render the configured access-token destination template.
fn render_access_token_template(template: &str, token: &str) -> Result<String, String> {
    render_template(template, |placeholder| match placeholder {
        "token" => Some(token.to_string()),
        _ => None,
    })
}

/// Render a `headers` canonical component template using `{name}` and
/// `{value}` substitutions.
fn render_named_value_template(template: &str, name: &str, value: &str) -> Result<String, String> {
    render_template(template, |token| match token {
        "name" => Some(name.to_string()),
        "value" => Some(value.to_string()),
        _ => None,
    })
}

/// Render a simple `{token}` template and error on unmatched or unresolved
/// placeholders.
fn render_template(
    template: &str,
    resolve: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut rendered = String::with_capacity(template.len());
    let mut current: Option<String> = None;

    for ch in template.chars() {
        match (&mut current, ch) {
            (None, '{') => current = Some(String::new()),
            (None, '}') => return Err("template contains unmatched `}`".to_string()),
            (None, other) => rendered.push(other),
            (Some(_), '{') => return Err("template contains nested `{`".to_string()),
            (Some(token), '}') => {
                let replacement = resolve(token.as_str()).ok_or_else(|| {
                    format!("template contains unknown or unresolved token `{{{token}}}`")
                })?;
                rendered.push_str(&replacement);
                current = None;
            }
            (Some(token), other) => token.push(other),
        }
    }

    if current.is_some() {
        return Err("template contains unterminated `{...` token".to_string());
    }

    Ok(rendered)
}

fn current_gmt_date() -> String {
    Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn current_iso8601_zulu() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn random_hex(byte_count: usize) -> String {
    let mut bytes = vec![0u8; byte_count];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_uuid_v4() -> String {
    Uuid::new_v4().to_string()
}

fn percent_encode_rfc3986(input: &str) -> String {
    utf8_percent_encode(input, RFC3986_ENCODE_SET).to_string()
}

fn host_header_value(url: &reqwest::Url) -> Result<String, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "HMAC upstream_host requires a host in the upstream URL".to_string())?;
    match url.port() {
        Some(port) => Ok(format!("{host}:{port}")),
        None => Ok(host.to_string()),
    }
}

// =============================================================================
// Generic access token cache
// =============================================================================

#[derive(PartialEq, Eq, Hash)]
struct AccessTokenKey {
    config: String,
}

static ACCESS_TOKEN_CACHE: std::sync::OnceLock<Arc<RwLock<HashMap<AccessTokenKey, CachedToken>>>> =
    std::sync::OnceLock::new();

fn access_token_cache() -> &'static Arc<RwLock<HashMap<AccessTokenKey, CachedToken>>> {
    ACCESS_TOKEN_CACHE.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

/// Fetch an access token using a nested HTTP request, reusing a cached token
/// until shortly before expiry.
async fn access_token(fetch: &AccessTokenFetchConfig) -> Result<String, String> {
    let key = AccessTokenKey {
        config: serde_json::to_string(fetch)
            .map_err(|e| format!("Access token cache key serialization failed: {e}"))?,
    };

    {
        let cache = access_token_cache().read().await;
        if let Some(cached) = cache.get(&key)
            && cached.expires_at > std::time::Instant::now() + std::time::Duration::from_secs(30)
        {
            return Ok(cached.access_token.clone());
        }
    }

    let fetched = fetch_access_token(fetch).await?;
    let ttl = fetched
        .expires_in_secs
        .saturating_sub(fetch.response.refresh_skew_seconds);
    let expires_at = std::time::Instant::now() + std::time::Duration::from_secs(ttl);

    {
        let mut cache = access_token_cache().write().await;
        cache.insert(
            key,
            CachedToken {
                access_token: fetched.access_token.clone(),
                expires_at,
            },
        );
    }

    Ok(fetched.access_token)
}

async fn fetch_access_token(fetch: &AccessTokenFetchConfig) -> Result<FetchedToken, String> {
    let mut prepared = PreparedUpstreamRequest::new(&fetch.url)?;
    let method = axum_method_from_spec(&fetch.method);

    for binding in &fetch.prepare {
        let value = resolve_hmac_prepare_value(&prepared.url, &[], &binding.value)?;
        apply_hmac_target(
            &mut prepared,
            &binding.target.kind,
            &binding.target.name,
            &value,
        );
    }

    if let Some(auth) = fetch.auth.as_deref() {
        apply_prepared_request_auth(&mut prepared, &method, &[], auth)?;
    }

    let client = upstream_http_client();
    let mut request = client.request(
        reqwest_method_from_spec(&fetch.method),
        prepared.url.clone(),
    );
    for (name, value) in &prepared.headers {
        request = request.header(name.as_str(), value);
    }

    let response = request.send().await.map_err(|e| {
        format!(
            "Access token request failed: {}",
            upstream_error_for_logging(&e)
        )
    })?;
    let status = response.status();
    let text = response.text().await.map_err(|e| {
        format!(
            "Access token response read failed: {}",
            upstream_error_for_logging(&e)
        )
    })?;
    if !status.is_success() {
        return Err(format!("Access token request failed with {status}"));
    }

    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid access token response: {e}"))?;
    let access_token =
        json_pointer_string(&body, &fetch.response.token_json_pointer, "access token")?;
    let expires_in_secs = access_token_ttl_seconds(&body, &fetch.response)?;

    Ok(FetchedToken {
        access_token,
        expires_in_secs,
    })
}

fn access_token_ttl_seconds(
    body: &serde_json::Value,
    response: &AccessTokenResponseConfig,
) -> Result<u64, String> {
    if let Some(pointer) = &response.expires_in_json_pointer {
        return json_pointer_u64(body, pointer, "access token expiry")
            .map(|seconds| seconds.max(1));
    }

    let expires_at = json_pointer_u64(
        body,
        response
            .expires_at_json_pointer
            .as_ref()
            .ok_or_else(|| "Access token response missing expiry pointer".to_string())?,
        "access token expiry timestamp",
    )?;

    match response.expires_at_format {
        pay_types::metering::AccessTokenExpiryFormat::UnixSeconds => {
            let now = Utc::now().timestamp().max(0) as u64;
            Ok(expires_at.saturating_sub(now).max(1))
        }
    }
}

fn json_pointer_string(
    body: &serde_json::Value,
    pointer: &str,
    label: &str,
) -> Result<String, String> {
    body.pointer(pointer)
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("Missing or invalid {label} at JSON Pointer `{pointer}`"))
}

fn json_pointer_u64(body: &serde_json::Value, pointer: &str, label: &str) -> Result<u64, String> {
    body.pointer(pointer)
        .and_then(|value| value.as_u64())
        .ok_or_else(|| format!("Missing or invalid {label} at JSON Pointer `{pointer}`"))
}

fn reqwest_method_from_spec(method: &HttpMethod) -> reqwest::Method {
    match method {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
        HttpMethod::Patch => reqwest::Method::PATCH,
        HttpMethod::Delete => reqwest::Method::DELETE,
    }
}

fn axum_method_from_spec(method: &HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
    }
}

// =============================================================================
// GCP OAuth2 token cache
// =============================================================================

/// Cached OAuth2 token with expiry.
struct CachedToken {
    access_token: String,
    expires_at: std::time::Instant,
}

/// A freshly-fetched token with the provider-reported lifetime.
pub(super) struct FetchedToken {
    pub(super) access_token: String,
    pub(super) expires_in_secs: u64,
}

/// Cache key: one entry per distinct (token_url, scopes, audience, client_id)
/// tuple. The metadata server returns different tokens for different scope
/// sets (and different audiences in identity mode), and standard OAuth2
/// providers key tokens by client. Caching them all under a single slot would
/// cause one upstream's token to evict another's. The client-secret identity
/// (env name plus a SHA-256 fingerprint of its value) is part of the key so a
/// token minted with one secret is never served to a route using another, and
/// rotating a secret invalidates its cached token.
#[derive(PartialEq, Eq, Hash)]
struct TokenKey {
    token_url: String,
    scopes: Vec<String>,
    audience: Option<String>,
    client_id_env: Option<String>,
    client_id: Option<String>,
    client_secret_env: Option<String>,
    client_secret_fingerprint: Option<[u8; 32]>,
}

static OAUTH2_TOKEN_CACHE: std::sync::OnceLock<Arc<RwLock<HashMap<TokenKey, CachedToken>>>> =
    std::sync::OnceLock::new();

fn token_cache() -> &'static Arc<RwLock<HashMap<TokenKey, CachedToken>>> {
    OAUTH2_TOKEN_CACHE.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

/// Fetch an OAuth2 access token, using a cached value if still valid.
async fn oauth2_token(
    token_url: &str,
    scopes: &[String],
    audience: Option<&str>,
    client_id_env: Option<&str>,
    client_secret_env: Option<&str>,
) -> Result<String, String> {
    let key = TokenKey {
        token_url: token_url.to_string(),
        scopes: scopes.to_vec(),
        audience: audience.map(str::to_string),
        client_id_env: client_id_env.map(str::to_string),
        client_id: client_id_env.and_then(|e| std::env::var(e).ok()),
        client_secret_env: client_secret_env.map(str::to_string),
        client_secret_fingerprint: client_secret_env
            .and_then(|e| std::env::var(e).ok())
            .map(|secret| Sha256::digest(secret.as_bytes()).into()),
    };

    // Check cache — require at least 30s of remaining life to avoid races
    // with in-flight upstream requests.
    {
        let cache = token_cache().read().await;
        if let Some(cached) = cache.get(&key)
            && cached.expires_at > std::time::Instant::now() + std::time::Duration::from_secs(30)
        {
            return Ok(cached.access_token.clone());
        }
    }

    let fetched = fetch_oauth2_token(
        token_url,
        scopes,
        audience,
        client_id_env,
        client_secret_env,
    )
    .await?;

    // Refresh 60s before the provider-reported expiry. Providers (especially
    // the GCP metadata server) may return a token that's already partially
    // used, so NEVER assume a fixed ~1h lifetime — always honour `expires_in`.
    let refresh_margin = 60;
    let ttl = fetched.expires_in_secs.saturating_sub(refresh_margin);
    let expires_at = std::time::Instant::now() + std::time::Duration::from_secs(ttl);

    {
        let mut cache = token_cache().write().await;
        cache.insert(
            key,
            CachedToken {
                access_token: fetched.access_token.clone(),
                expires_at,
            },
        );
    }

    Ok(fetched.access_token)
}

async fn fetch_oauth2_token(
    token_url: &str,
    scopes: &[String],
    audience: Option<&str>,
    client_id_env: Option<&str>,
    client_secret_env: Option<&str>,
) -> Result<FetchedToken, String> {
    let client = upstream_http_client();

    // Special: GCP metadata server.
    if token_url == "gcp_metadata" {
        return fetch_gcp_metadata_token(&client, scopes).await;
    }
    if token_url == "gcp_metadata_identity" {
        let audience = audience.ok_or_else(|| {
            "oauth2.audience is required for token_url `gcp_metadata_identity`".to_string()
        })?;
        return fetch_gcp_metadata_identity_token(&client, audience).await;
    }

    // Standard OAuth2 client_credentials grant.
    let client_id = client_id_env
        .and_then(|e| std::env::var(e).ok())
        .ok_or("OAuth2 client_id env var not set")?;
    let client_secret = client_secret_env
        .and_then(|e| std::env::var(e).ok())
        .ok_or("OAuth2 client_secret env var not set")?;

    let mut params = vec![
        ("grant_type", "client_credentials".to_string()),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ];
    if !scopes.is_empty() {
        params.push(("scope", scopes.join(" ")));
    }

    let resp = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            format!(
                "OAuth2 token request failed: {}",
                upstream_error_for_logging(&e)
            )
        })?;

    let body: serde_json::Value = resp.json().await.map_err(|e| {
        format!(
            "Invalid OAuth2 response: {}",
            upstream_error_for_logging(&e)
        )
    })?;

    let access_token = body["access_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "OAuth2 response did not contain an access_token".to_string())?;
    let expires_in_secs = body["expires_in"].as_u64().unwrap_or(3600);

    Ok(FetchedToken {
        access_token,
        expires_in_secs,
    })
}

/// Fetch token from GCP metadata server (Cloud Run / GCE).
/// Falls back to Application Default Credentials for local dev.
async fn fetch_gcp_metadata_token(
    client: &reqwest::Client,
    scopes: &[String],
) -> Result<FetchedToken, String> {
    let scopes_param = scopes.join(",");

    // 1. Metadata server.
    let url = format!(
        "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token?scopes={scopes_param}"
    );
    if let Ok(resp) = client
        .get(&url)
        .header("Metadata-Flavor", "Google")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        && resp.status().is_success()
        && let Ok(body) = resp.json::<serde_json::Value>().await
        && let Some(token) = body["access_token"].as_str()
    {
        tracing::debug!("OAuth2 token from GCP metadata server");
        // The metadata server returns its own cached token and only mints a
        // fresh one shortly before expiry, so `expires_in` is the remaining
        // lifetime of that shared token — not a fresh 1h window.
        let expires_in_secs = body["expires_in"].as_u64().unwrap_or(3600);
        return Ok(FetchedToken {
            access_token: token.to_string(),
            expires_in_secs,
        });
    }

    // 2. Application Default Credentials (local dev).
    let adc_path = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.config/gcloud/application_default_credentials.json")
    });
    let adc_content = std::fs::read_to_string(&adc_path)
        .map_err(|e| format!("No metadata server and can't read ADC at {adc_path}: {e}"))?;
    let adc: serde_json::Value =
        serde_json::from_str(&adc_content).map_err(|e| format!("Invalid ADC: {e}"))?;

    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", adc["client_id"].as_str().unwrap_or_default()),
            (
                "client_secret",
                adc["client_secret"].as_str().unwrap_or_default(),
            ),
            (
                "refresh_token",
                adc["refresh_token"].as_str().unwrap_or_default(),
            ),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|e| {
            format!(
                "ADC token refresh failed: {}",
                upstream_error_for_logging(&e)
            )
        })?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid token response: {}", upstream_error_for_logging(&e)))?;

    let access_token = body["access_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "ADC response did not contain an access_token".to_string())?;
    let expires_in_secs = body["expires_in"].as_u64().unwrap_or(3600);

    tracing::debug!("OAuth2 token from ADC");
    Ok(FetchedToken {
        access_token,
        expires_in_secs,
    })
}

/// Fetch an audience-bound OIDC identity token from the GCP metadata server
/// (Cloud Run / GCE), used to invoke IAM-protected services such as private
/// Cloud Run. Unlike access tokens there is no ADC fallback: user credentials
/// cannot mint audience-bound identity tokens.
pub(super) async fn fetch_gcp_metadata_identity_token(
    client: &reqwest::Client,
    audience: &str,
) -> Result<FetchedToken, String> {
    // Cloud Run's platform-local metadata endpoint uses HTTP, not public
    // HTTPS. This fixed hostname is not the external service receiving the
    // token; that service is authenticated separately with the bound audience.
    fetch_gcp_metadata_identity_token_at(
        client,
        audience,
        "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/identity",
    )
    .await
}

pub(super) async fn fetch_gcp_metadata_identity_token_at(
    client: &reqwest::Client,
    audience: &str,
    endpoint: &str,
) -> Result<FetchedToken, String> {
    let resp = client
        .get(endpoint)
        .query(&[("audience", audience)])
        .header("Metadata-Flavor", "Google")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .map_err(|e| {
            let e = upstream_error_for_logging(&e);
            format!(
                "GCP metadata server unreachable ({e}): identity tokens require running on GCP (Cloud Run/GCE); there is no ADC fallback"
            )
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(format!("GCP metadata identity endpoint returned {status}"));
    }

    // Bound chunked bodies too; metadata credentials must never cause an
    // unbounded allocation before the authenticated resolver request.
    let bytes = super::deployment_policy::read_bounded_response(resp, 16 * 1024)
        .await
        .map_err(|_| "Failed to read bounded identity token".to_string())?;
    let token = std::str::from_utf8(&bytes)
        .map_err(|_| "Identity token is not UTF-8".to_string())?
        .trim()
        .to_string();

    // The /identity endpoint returns a raw JWT with no expiry envelope —
    // read the remaining lifetime from the token's own `exp` claim.
    let exp = jwt_exp_claim(&token)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("System clock before UNIX epoch: {e}"))?
        .as_secs();

    tracing::debug!("OIDC identity token from GCP metadata server");
    Ok(FetchedToken {
        access_token: token,
        expires_in_secs: exp.saturating_sub(now),
    })
}

/// Extract the `exp` claim (unix seconds) from a JWT without verifying the
/// signature — the token is opaque to us; only its remaining lifetime matters
/// for the cache.
fn jwt_exp_claim(jwt: &str) -> Result<u64, String> {
    let mut segments = jwt.split('.');
    let payload_b64 = match (segments.next(), segments.next(), segments.next()) {
        (Some(_), Some(payload), Some(_)) => payload,
        _ => return Err("identity token is not a JWT (expected 3 dot-separated segments)".into()),
    };
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| format!("identity token payload is not base64url: {e}"))?;
    let claims: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|e| format!("identity token payload is not JSON: {e}"))?;
    claims["exp"]
        .as_u64()
        .ok_or_else(|| "identity token has no numeric `exp` claim".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_types::metering::{
        AccessTokenFetchConfig, AccessTokenInjectConfig, AccessTokenResponseConfig,
        HmacCanonicalComponent, HmacCanonicalConfig, HmacDigestAlgorithm, HmacEncoding,
        HmacPrepareBinding, HmacPrepareValue, HmacQueryStyle, HmacSignatureConfig,
        HmacSignatureDestination, HmacStringEncoding, HmacTarget, HmacTargetType,
        HmacTimestampFormat, HttpMethod, RoutingConfig,
    };

    /// Unit tests exercise upstream preparation for an already-admitted request.
    #[allow(clippy::result_large_err)]
    async fn forward_request(
        api: &ApiSpec,
        method: Method,
        uri: &Uri,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<Response, Response> {
        let route = UpstreamRoute::for_tests(api, &method, uri.path());
        super::forward_request(api, &route, method, uri, headers, body).await
    }

    #[allow(clippy::result_large_err)]
    async fn prepare_upstream_with_identity(
        api: &ApiSpec,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        body: &[u8],
        identity: Option<&TrustedPaymentIdentity>,
    ) -> Result<UpstreamPlan, Response> {
        let route = UpstreamRoute::for_tests(api, method, uri.path());
        super::prepare_upstream_with_identity(api, &route, method, uri, headers, body, identity)
            .await
    }

    #[test]
    fn upstream_log_url_redacts_every_query_value() {
        let url = reqwest::Url::parse(
            "https://api.example.com/v1?key=super-secret&cursor=also-sensitive",
        )
        .unwrap();

        let safe = upstream_url_for_logging(&url);

        assert!(safe.contains("key=%5BREDACTED%5D"));
        assert!(safe.contains("cursor=%5BREDACTED%5D"));
        assert!(!safe.contains("super-secret"));
        assert!(!safe.contains("also-sensitive"));
    }

    #[test]
    fn prepared_request_debug_never_exposes_credentials() {
        let request = PreparedUpstreamRequest {
            url: reqwest::Url::parse("https://api.example.com/v1?key=query-secret").unwrap(),
            headers: vec![(
                "authorization".to_string(),
                "Bearer header-secret".to_string(),
            )],
            url_credentials: UrlCredentials::default(),
        };

        let debug = format!("{request:?}");

        assert!(debug.contains("authorization"));
        assert!(!debug.contains("query-secret"));
        assert!(!debug.contains("header-secret"));
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn make_api(subdomain: &str) -> ApiSpec {
        ApiSpec {
            name: "test".to_string(),
            subdomain: subdomain.to_string(),
            title: "Test".to_string(),
            description: "".to_string(),
            category: pay_types::metering::ApiCategory::AiMl,
            version: "1.0".to_string(),
            env: std::collections::HashMap::new(),
            routing: RoutingConfig::Proxy {
                url: "https://api.example.com".to_string(),
                path_rewrites: vec![],
                auth: None,
            },
            accounting: pay_types::metering::AccountingMode::Pooled,
            endpoints: vec![],
            free_tier: None,
            quotas: None,
            notes: None,
            operator: None,
            session: None,
            batch_settlement: None,
            recipients: std::collections::HashMap::new(),
        }
    }

    /// A client-voucher session admitted at cumulative 1_000 requests `path`
    /// from a JSON upstream reporting 50 completion tokens. Returns the first
    /// body bytes released within 500ms, if any.
    async fn client_voucher_first_bytes(path: &str) -> Option<Bytes> {
        use futures_util::StreamExt;
        let upstream = axum::Router::new().fallback(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"text":"protected-content","usage":{"completion_tokens":50}}"#,
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, upstream).await });

        let metering = |dimension: serde_json::Value| {
            serde_json::from_value(serde_json::json!({
                "schemes": ["mpp-session"],
                "dimensions": [dimension],
            }))
            .unwrap()
        };
        let mut api = make_api("test");
        api.routing = RoutingConfig::Proxy {
            url: format!("http://{addr}"),
            path_rewrites: vec![],
            auth: None,
        };
        let mut metered = endpoint_with_routing("v1/metered", None);
        metered.metering = Some(metering(serde_json::json!({
            "direction": "output", "unit": "tokens", "scale": 1,
            "tiers": [{"price_usd": 0.000001}],
            "meter": {"source": "response_json", "path": "/usage/completion_tokens"}
        })));
        let mut flat = endpoint_with_routing("v1/flat", None);
        flat.metering = Some(metering(serde_json::json!({
            "direction": "usage", "unit": "requests", "scale": 1,
            "tiers": [{"price_usd": 0.001}]
        })));
        api.endpoints = vec![metered, flat];

        let key = || solana_pubkey::Pubkey::new_unique().to_string();
        let handle = std::sync::Arc::new(crate::server::session::SessionMpp::new(
            pay_kit::mpp::server::session::SessionConfig {
                operator: key(),
                recipient: key(),
                currency: key(),
                network: "localnet".to_string(),
                ..Default::default()
            },
            "test-secret",
        ));
        let uri: Uri = format!("/{path}").parse().unwrap();
        let route = UpstreamRoute::for_tests(&api, &Method::POST, path);
        let served = async {
            let response = forward_request_with_session_metering(
                &api,
                &route,
                Method::POST,
                &uri,
                &HeaderMap::new(),
                Bytes::new(),
                Some(SessionStreamContext::new(handle, key(), 1_000)),
            )
            .await
            .unwrap();
            response.into_body().into_data_stream().next().await
        };
        tokio::time::timeout(std::time::Duration::from_millis(500), served)
            .await
            .ok()
            .flatten()
            .map(|chunk| chunk.unwrap())
    }

    #[tokio::test]
    async fn client_voucher_response_is_withheld_until_its_usage_is_covered() {
        assert_eq!(client_voucher_first_bytes("v1/metered").await, None);
    }

    #[tokio::test]
    async fn client_voucher_request_priced_response_is_released() {
        let served = client_voucher_first_bytes("v1/flat").await.unwrap();
        assert!(served.starts_with(b"{\"text\":\"protected-content\""));
    }

    #[tokio::test]
    async fn path_rewrite_secrets_never_reach_upstream_logs() {
        // SAFETY: test-only variable read by this test alone.
        unsafe { std::env::set_var("_TEST_PATH_REWRITE_SECRET", "user/p4ss") };
        let mut api = make_api("test");
        if let RoutingConfig::Proxy { path_rewrites, .. } = &mut api.routing {
            path_rewrites.push(pay_types::metering::PathRewrite {
                prefix: "live/{credentials}".to_string(),
                env: "_TEST_PATH_REWRITE_SECRET".to_string(),
            });
        }
        let prepared = prepare_upstream_with_identity(
            &api,
            &Method::GET,
            &"/live/x/stream-1?page=2".parse().unwrap(),
            &HeaderMap::new(),
            &[],
            None,
        )
        .await
        .unwrap();
        let UpstreamPlan::Forward(prepared) = prepared else {
            panic!("expected proxy forwarding");
        };
        assert!(prepared.url.path().contains("p4ss"), "{}", prepared.url);

        let logged = prepared.url_for_logging();
        assert_eq!(
            logged,
            "https://api.example.com/live/[REDACTED]/[REDACTED]/stream-1?page=%5BREDACTED%5D"
        );
        assert!(!format!("{prepared:?}").contains("p4ss"));
        let quoted = format!("error sending request for url ({})", prepared.url);
        assert!(!prepared.redact_for_logging(&quoted).contains("p4ss"));
        // Positive control: an unrelated segment keeps its value.
        assert!(logged.contains("stream-1"));
    }

    fn credentials(values: &[&str]) -> UrlCredentials {
        let mut credentials = UrlCredentials::default();
        for value in values {
            credentials.push(value);
        }
        credentials
    }

    #[test]
    fn url_credentials_are_scrubbed_from_relayed_urls() {
        let credentials = credentials(&["s3cr3t", "a+b/c"]);
        for (location, scrubbed) in [
            (
                "https://up.example/next?key=s3cr3t&page=2",
                Some("https://up.example/next?page=2"),
            ),
            ("/next?sig=a%2Bb%2Fc", Some("/next")),
            ("/next?sig=a%2Bb%2Fc#top", Some("/next#top")),
            // A credential outside the query cannot be removed: drop it.
            ("https://up.example/s3cr3t/next", None),
            // Positive control: unrelated redirects are relayed untouched.
            ("/next?page=2", Some("/next?page=2")),
        ] {
            assert_eq!(
                credentials.scrub_url(location).as_deref(),
                scrubbed,
                "{location}"
            );
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::LOCATION,
            HeaderValue::from_static("/next?key=s3cr3t"),
        );
        headers.insert(
            axum::http::header::CONTENT_LOCATION,
            HeaderValue::from_static("/s3cr3t"),
        );
        headers.insert("x-other", HeaderValue::from_static("s3cr3t-unrelated"));
        credentials.scrub_response_headers(&mut headers);
        assert_eq!(headers.get(axum::http::header::LOCATION).unwrap(), "/next");
        assert!(headers.get(axum::http::header::CONTENT_LOCATION).is_none());
        assert!(headers.get("x-other").is_some());
    }

    #[tokio::test]
    async fn upstream_redirects_never_relay_the_query_api_key() {
        let upstream = axum::Router::new().fallback(|uri: Uri| async move {
            (
                StatusCode::FOUND,
                [(
                    axum::http::header::LOCATION,
                    format!("/landing?{}", uri.query().unwrap_or("")),
                )],
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, upstream).await });
        let mut api = api_with_auth(Some(AuthConfig::QueryParam {
            key: "key".to_string(),
            // PATH is always set; its value stands in for the API key.
            value_from_env: "PATH".to_string(),
        }));
        if let RoutingConfig::Proxy { url, .. } = &mut api.routing {
            *url = format!("http://{addr}");
        }
        let uri: Uri = "/v1/data?page=2".parse().unwrap();

        let response = forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FOUND);
        let location = response.headers()[axum::http::header::LOCATION]
            .to_str()
            .unwrap();
        assert_eq!(location, "/landing?page=2");
    }

    fn api_with_auth(auth: Option<AuthConfig>) -> ApiSpec {
        let mut api = make_api("test");
        if let RoutingConfig::Proxy { auth: a, .. } = &mut api.routing {
            *a = auth.map(Box::new);
        }
        api
    }

    #[test]
    fn routing_signs_request_body_detects_body_digest() {
        // Alibaba-style HMAC includes a Content-MD5 `BodyDigest` prepare binding,
        // so it signs the request body → unsupported on the streaming data plane.
        let api = api_with_auth(Some(alibaba_hmac_auth_config(true)));
        assert!(routing_signs_request_body(
            &api,
            &UpstreamRoute::for_tests(&api, &Method::POST, "v1/anything")
        ));
    }

    #[test]
    fn routing_signs_request_body_false_when_body_untouched() {
        // No upstream auth at all.
        let api = make_api("test");
        assert!(!routing_signs_request_body(
            &api,
            &UpstreamRoute::for_tests(&api, &Method::POST, "v1/anything")
        ));
        // Header auth never reads the body.
        let header = api_with_auth(Some(AuthConfig::Header {
            key: "Authorization".to_string(),
            prefix: Some("Bearer ".to_string()),
            value_from_env: "_TEST_KEY".to_string(),
        }));
        assert!(!routing_signs_request_body(
            &header,
            &UpstreamRoute::for_tests(&header, &Method::POST, "v1/anything")
        ));
    }

    fn alibaba_hmac_auth_config(fixed_values: bool) -> AuthConfig {
        let date_value = if fixed_values {
            HmacPrepareValue::Literal {
                value: "Wed, 26 Aug 2015 17:01:00 GMT".to_string(),
            }
        } else {
            HmacPrepareValue::Timestamp {
                format: HmacTimestampFormat::Rfc1123Gmt,
            }
        };
        let nonce_value = if fixed_values {
            HmacPrepareValue::Literal {
                value: "test-nonce".to_string(),
            }
        } else {
            HmacPrepareValue::RandomHex { bytes: 16 }
        };

        AuthConfig::Hmac {
            algorithm: HmacAlgorithm::Sha1,
            secret_from_env: "_TEST_ALIBABA_ACCESS_KEY_SECRET".to_string(),
            secret_suffix: None,
            key_id_from_env: Some("_TEST_ALIBABA_ACCESS_KEY_ID".to_string()),
            prepare: vec![
                HmacPrepareBinding {
                    target: HmacTarget {
                        kind: HmacTargetType::Header,
                        name: "Accept".to_string(),
                    },
                    value: HmacPrepareValue::Literal {
                        value: "application/json".to_string(),
                    },
                },
                HmacPrepareBinding {
                    target: HmacTarget {
                        kind: HmacTargetType::Header,
                        name: "Content-Type".to_string(),
                    },
                    value: HmacPrepareValue::Literal {
                        value: "application/json;charset=utf-8".to_string(),
                    },
                },
                HmacPrepareBinding {
                    target: HmacTarget {
                        kind: HmacTargetType::Header,
                        name: "Content-MD5".to_string(),
                    },
                    value: HmacPrepareValue::BodyDigest {
                        algorithm: HmacDigestAlgorithm::Md5,
                        encoding: HmacEncoding::Base64,
                    },
                },
                HmacPrepareBinding {
                    target: HmacTarget {
                        kind: HmacTargetType::Header,
                        name: "Date".to_string(),
                    },
                    value: date_value,
                },
                HmacPrepareBinding {
                    target: HmacTarget {
                        kind: HmacTargetType::Header,
                        name: "Host".to_string(),
                    },
                    value: HmacPrepareValue::UpstreamHost {},
                },
                HmacPrepareBinding {
                    target: HmacTarget {
                        kind: HmacTargetType::Header,
                        name: "x-acs-signature-nonce".to_string(),
                    },
                    value: nonce_value,
                },
                HmacPrepareBinding {
                    target: HmacTarget {
                        kind: HmacTargetType::Header,
                        name: "x-acs-signature-method".to_string(),
                    },
                    value: HmacPrepareValue::Literal {
                        value: "HMAC-SHA1".to_string(),
                    },
                },
                HmacPrepareBinding {
                    target: HmacTarget {
                        kind: HmacTargetType::Header,
                        name: "x-acs-version".to_string(),
                    },
                    value: HmacPrepareValue::Literal {
                        value: "2019-01-02".to_string(),
                    },
                },
            ],
            canonical: HmacCanonicalConfig {
                join_with: "\n".to_string(),
                components: vec![
                    HmacCanonicalComponent::Method {},
                    HmacCanonicalComponent::Header {
                        name: "Accept".to_string(),
                    },
                    HmacCanonicalComponent::Header {
                        name: "Content-MD5".to_string(),
                    },
                    HmacCanonicalComponent::Header {
                        name: "Content-Type".to_string(),
                    },
                    HmacCanonicalComponent::Header {
                        name: "Date".to_string(),
                    },
                    HmacCanonicalComponent::Headers {
                        names: vec![
                            "x-acs-signature-method".to_string(),
                            "x-acs-signature-nonce".to_string(),
                            "x-acs-version".to_string(),
                        ],
                        join_with: "\n".to_string(),
                        format: "{name}:{value}".to_string(),
                    },
                    HmacCanonicalComponent::Path {},
                ],
            },
            signature: HmacSignatureConfig {
                encoding: HmacEncoding::Base64,
                destination: HmacSignatureDestination {
                    kind: HmacTargetType::Header,
                    name: "Authorization".to_string(),
                    template: "acs {key_id}:{signature}".to_string(),
                },
            },
        }
    }

    fn isi_access_token_auth_config(token_url: &str) -> AuthConfig {
        AuthConfig::AccessToken {
            prepare: vec![HmacPrepareBinding {
                target: HmacTarget {
                    kind: HmacTargetType::QueryParam,
                    name: "appkey".to_string(),
                },
                value: HmacPrepareValue::Env {
                    from_env: "_TEST_ISI_APP_KEY".to_string(),
                },
            }],
            fetch: AccessTokenFetchConfig {
                url: token_url.to_string(),
                method: HttpMethod::Get,
                prepare: vec![
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "AccessKeyId".to_string(),
                        },
                        value: HmacPrepareValue::Env {
                            from_env: "_TEST_ISI_ACCESS_KEY_ID".to_string(),
                        },
                    },
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "Action".to_string(),
                        },
                        value: HmacPrepareValue::Literal {
                            value: "CreateToken".to_string(),
                        },
                    },
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "Format".to_string(),
                        },
                        value: HmacPrepareValue::Literal {
                            value: "JSON".to_string(),
                        },
                    },
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "RegionId".to_string(),
                        },
                        value: HmacPrepareValue::Literal {
                            value: "ap-southeast-1".to_string(),
                        },
                    },
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "SignatureMethod".to_string(),
                        },
                        value: HmacPrepareValue::Literal {
                            value: "HMAC-SHA1".to_string(),
                        },
                    },
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "SignatureNonce".to_string(),
                        },
                        value: HmacPrepareValue::Literal {
                            value: "4e1c1a27-4eaa-4df1-bf8f-78d4e5b79c3d".to_string(),
                        },
                    },
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "SignatureVersion".to_string(),
                        },
                        value: HmacPrepareValue::Literal {
                            value: "1.0".to_string(),
                        },
                    },
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "Timestamp".to_string(),
                        },
                        value: HmacPrepareValue::Literal {
                            value: "2019-04-18T08:32:31Z".to_string(),
                        },
                    },
                    HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::QueryParam,
                            name: "Version".to_string(),
                        },
                        value: HmacPrepareValue::Literal {
                            value: "2019-07-17".to_string(),
                        },
                    },
                ],
                auth: Some(Box::new(AuthConfig::Hmac {
                    algorithm: HmacAlgorithm::Sha1,
                    secret_from_env: "_TEST_ISI_ACCESS_KEY_SECRET".to_string(),
                    secret_suffix: Some("&".to_string()),
                    key_id_from_env: None,
                    prepare: vec![],
                    canonical: HmacCanonicalConfig {
                        join_with: "".to_string(),
                        components: vec![
                            HmacCanonicalComponent::Method {},
                            HmacCanonicalComponent::Literal {
                                value: "&%2F&".to_string(),
                            },
                            HmacCanonicalComponent::Query {
                                style: HmacQueryStyle::SortedPairs,
                                encoding: HmacStringEncoding::PercentRfc3986,
                            },
                        ],
                    },
                    signature: HmacSignatureConfig {
                        encoding: HmacEncoding::Base64,
                        destination: HmacSignatureDestination {
                            kind: HmacTargetType::QueryParam,
                            name: "Signature".to_string(),
                            template: "{signature}".to_string(),
                        },
                    },
                })),
                response: AccessTokenResponseConfig {
                    token_json_pointer: "/Token/Id".to_string(),
                    expires_at_json_pointer: Some("/Token/ExpireTime".to_string()),
                    expires_in_json_pointer: None,
                    expires_at_format: pay_types::metering::AccessTokenExpiryFormat::UnixSeconds,
                    refresh_skew_seconds: 60,
                },
            },
            inject: AccessTokenInjectConfig {
                target: HmacTarget {
                    kind: HmacTargetType::Header,
                    name: "X-NLS-Token".to_string(),
                },
                template: "{token}".to_string(),
            },
        }
    }

    #[test]
    fn resolve_api_finds_matching_subdomain() {
        let apis = vec![make_api("vision"), make_api("translate")];
        let result = resolve_api(&apis, "vision.agents.solana.com");
        assert!(result.is_some());
        assert_eq!(result.unwrap().subdomain, "vision");
    }

    #[test]
    fn resolve_api_no_match() {
        let apis = vec![make_api("vision")];
        let result = resolve_api(&apis, "translate.agents.solana.com");
        assert!(result.is_none());
    }

    #[test]
    fn resolve_api_empty_list() {
        let apis: Vec<ApiSpec> = vec![];
        assert!(resolve_api(&apis, "vision.agents.solana.com").is_none());
    }

    #[test]
    fn error_response_has_correct_status() {
        let resp = error_response(StatusCode::BAD_GATEWAY, "upstream error");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn error_response_has_json_content_type() {
        let resp = error_response(StatusCode::INTERNAL_SERVER_ERROR, "oops");
        let ct = resp.headers().get("content-type").unwrap();
        assert_eq!(ct, "application/json");
    }

    #[test]
    fn strip_headers_contains_expected() {
        assert!(STRIP_HEADERS.contains(&"host"));
        assert!(STRIP_HEADERS.contains(&"authorization"));
        assert!(STRIP_HEADERS.contains(&"connection"));
        // x402 credential headers must never be forwarded upstream.
        assert!(STRIP_HEADERS.contains(&"payment-signature"));
        assert!(STRIP_HEADERS.contains(&"x-payment"));
        assert!(STRIP_HEADERS.contains(&"x-pay-verified-payer"));
        assert!(STRIP_HEADERS.contains(&"x-pay-verified-channel"));
        assert!(STRIP_HEADERS.contains(&"x-pay-original-host"));
    }

    #[tokio::test]
    async fn caller_proxy_routing_headers_never_reach_the_upstream() {
        let api = make_api("test");
        let mut headers = HeaderMap::new();
        for name in [
            "forwarded",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-forwarded-port",
            "x-forwarded-prefix",
            "x-forwarded-for",
            "x-real-ip",
        ] {
            headers.insert(name, HeaderValue::from_static("other-tenant.example"));
        }
        // Positive control: ordinary caller headers still pass through.
        headers.insert("x-request-id", HeaderValue::from_static("abc"));
        let UpstreamPlan::Forward(prepared) = prepare_upstream_with_identity(
            &api,
            &Method::POST,
            &"/summary".parse().unwrap(),
            &headers,
            &[],
            None,
        )
        .await
        .unwrap() else {
            panic!("expected proxy forwarding");
        };
        for (name, _) in &headers {
            if name.as_str() != "x-request-id" {
                assert_eq!(prepared.header_value(name.as_str()), None, "{name}");
            }
        }
        assert_eq!(prepared.header_value("x-request-id"), Some("abc"));
    }

    #[tokio::test]
    async fn trusted_payment_identity_replaces_spoofed_forwarding_headers() {
        let api = make_api("test");
        let mut headers = HeaderMap::new();
        headers.insert("x-pay-verified-payer", HeaderValue::from_static("attacker"));
        headers.insert(
            "x-pay-verified-channel",
            HeaderValue::from_static("attacker"),
        );
        headers.insert(
            "x-pay-original-host",
            HeaderValue::from_static("evil.example"),
        );
        let identity = TrustedPaymentIdentity {
            payer: Some("trusted-payer".into()),
            channel_id: Some("trusted-channel".into()),
            original_host: Some("worker.cpu.gcp.gateway-402.com".into()),
            payment_policy: None,
        };
        let UpstreamPlan::Forward(prepared) = prepare_upstream_with_identity(
            &api,
            &Method::POST,
            &"/summary".parse().unwrap(),
            &headers,
            &[],
            Some(&identity),
        )
        .await
        .unwrap() else {
            panic!("expected proxy forwarding");
        };
        assert_eq!(
            prepared.header_value("x-pay-verified-payer"),
            Some("trusted-payer")
        );
        assert_eq!(
            prepared.header_value("x-pay-verified-channel"),
            Some("trusted-channel")
        );
        assert_eq!(
            prepared.header_value("x-pay-original-host"),
            Some("worker.cpu.gcp.gateway-402.com")
        );

        let identity = TrustedPaymentIdentity {
            payer: None,
            channel_id: None,
            original_host: Some("worker.cpu.gcp.gateway-402.com".into()),
            payment_policy: None,
        };
        let UpstreamPlan::Forward(prepared) = prepare_upstream_with_identity(
            &api,
            &Method::POST,
            &"/summary".parse().unwrap(),
            &headers,
            &[],
            Some(&identity),
        )
        .await
        .unwrap() else {
            panic!("expected proxy forwarding");
        };
        assert_eq!(prepared.header_value("x-pay-verified-payer"), None);
        assert_eq!(prepared.header_value("x-pay-verified-channel"), None);
        assert_eq!(
            prepared.header_value("x-pay-original-host"),
            Some("worker.cpu.gcp.gateway-402.com")
        );
        assert_eq!(prepared.header_value("x-pay-payment-policy"), None);
    }

    #[tokio::test]
    async fn admitted_payment_policy_replaces_a_spoofed_policy_header() {
        let api = make_api("test");
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-pay-payment-policy",
            HeaderValue::from_static(r#"{"version":999}"#),
        );
        let admitted =
            r#"{"deployment":{"hostname":"w.example"},"version":3,"price_micro_usd":10}"#;
        let identity = TrustedPaymentIdentity {
            payer: Some("trusted-payer".into()),
            payment_policy: Some(admitted.into()),
            ..Default::default()
        };
        let UpstreamPlan::Forward(prepared) = prepare_upstream_with_identity(
            &api,
            &Method::POST,
            &"/summary".parse().unwrap(),
            &headers,
            &[],
            Some(&identity),
        )
        .await
        .unwrap() else {
            panic!("expected proxy forwarding");
        };
        assert_eq!(
            prepared.header_value("x-pay-payment-policy"),
            Some(admitted)
        );

        // Without an admitted policy the caller's value never reaches upstream.
        let UpstreamPlan::Forward(prepared) = prepare_upstream_with_identity(
            &api,
            &Method::POST,
            &"/summary".parse().unwrap(),
            &headers,
            &[],
            Some(&TrustedPaymentIdentity::default()),
        )
        .await
        .unwrap() else {
            panic!("expected proxy forwarding");
        };
        assert_eq!(prepared.header_value("x-pay-payment-policy"), None);
    }

    #[test]
    #[serial_test::serial(alibaba_hmac_env)]
    fn generic_hmac_reproduces_alibaba_signature() {
        let body = br#"{"FormatType":"text","SourceLanguage":"en","TargetLanguage":"zh","SourceText":"Hello"}"#;
        let mut prepared = PreparedUpstreamRequest::new(
            "https://mt.cn-hangzhou.aliyuncs.com/api/translate/web/general",
        )
        .unwrap();
        let auth = alibaba_hmac_auth_config(true);
        let AuthConfig::Hmac {
            algorithm,
            secret_from_env,
            secret_suffix,
            key_id_from_env,
            prepare,
            canonical,
            signature,
        } = &auth
        else {
            panic!("expected HMAC auth config");
        };

        unsafe { std::env::set_var("_TEST_ALIBABA_ACCESS_KEY_ID", "test-id") };
        unsafe { std::env::set_var("_TEST_ALIBABA_ACCESS_KEY_SECRET", "test-secret") };

        apply_hmac_auth(
            &mut prepared,
            &Method::POST,
            body,
            &HmacAuthConfigRef {
                algorithm,
                secret_from_env,
                secret_suffix: secret_suffix.as_deref(),
                key_id_from_env: key_id_from_env.as_deref(),
                prepare,
                canonical,
                signature,
            },
        )
        .unwrap();

        unsafe { std::env::remove_var("_TEST_ALIBABA_ACCESS_KEY_ID") };
        unsafe { std::env::remove_var("_TEST_ALIBABA_ACCESS_KEY_SECRET") };

        assert_eq!(
            prepared.header_value("Content-MD5"),
            Some("jCZbaw/l8wB5VbGcfBmH0w==")
        );
        assert_eq!(
            prepared.header_value("Host"),
            Some("mt.cn-hangzhou.aliyuncs.com")
        );
        assert_eq!(
            prepared.header_value("Authorization").unwrap(),
            "acs test-id:3GVRsoc2POHOKxGKNukivfy9cbE="
        );
    }

    #[test]
    fn canonical_query_component_supports_raw_and_sorted_pairs() {
        let raw = canonical_query_component(
            &reqwest::Url::parse("https://example.com/v1/test?b=2&a=1&a=0").unwrap(),
            &HmacQueryStyle::Raw,
            &HmacStringEncoding::None,
        )
        .unwrap();
        let sorted = canonical_query_component(
            &reqwest::Url::parse("https://example.com/v1/test?b=2&a=1&a=0").unwrap(),
            &HmacQueryStyle::SortedPairs,
            &HmacStringEncoding::None,
        )
        .unwrap();

        assert_eq!(raw, "b=2&a=1&a=0");
        assert_eq!(sorted, "a=0&a=1&b=2");
    }

    #[test]
    fn canonical_query_component_supports_percent_encoded_rendering() {
        let encoded = canonical_query_component(
            &reqwest::Url::parse("https://example.com/v1/test?text=hello%20world&lang=en").unwrap(),
            &HmacQueryStyle::SortedPairs,
            &HmacStringEncoding::PercentRfc3986,
        )
        .unwrap();

        assert_eq!(encoded, "lang%3Den%26text%3Dhello%2520world");
    }

    #[test]
    fn random_uuid_v4_generates_rfc4122_version_4_uuid() {
        let uuid = Uuid::parse_str(&random_uuid_v4()).expect("generated UUID should parse");
        assert_eq!(uuid.get_version(), Some(uuid::Version::Random));
        assert_eq!(uuid.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn encode_bytes_supports_base64_and_hex() {
        assert_eq!(encode_bytes(b"abc", &HmacEncoding::Base64), "YWJj");
        assert_eq!(encode_bytes(b"abc", &HmacEncoding::Hex), "616263");
    }

    #[test]
    fn build_hmac_canonical_string_rejects_missing_header_component() {
        let prepared = PreparedUpstreamRequest::new("https://example.com/v1/check").unwrap();
        let canonical = HmacCanonicalConfig {
            join_with: "\n".to_string(),
            components: vec![HmacCanonicalComponent::Header {
                name: "Date".to_string(),
            }],
        };

        let err = build_hmac_canonical_string(&prepared, &Method::GET, &canonical).unwrap_err();
        assert!(err.contains("header `Date` is missing"));
    }

    /// Spin up a one-shot axum server, return its base URL.
    async fn spawn_upstream(handler: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            axum::serve(listener, handler).await.ok();
        });
        // Give the server a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        (url, handle)
    }

    #[tokio::test]
    async fn forward_request_get() {
        let app = axum::Router::new().route(
            "/v1/test",
            axum::routing::get(|| async { "hello from upstream" }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/test").parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"hello from upstream");
    }

    #[tokio::test]
    async fn forward_request_injects_cached_access_token_auth() {
        let token_calls = Arc::new(AtomicUsize::new(0));
        let token_query = Arc::new(Mutex::new(String::new()));
        let token_calls_capture = Arc::clone(&token_calls);
        let token_query_capture = Arc::clone(&token_query);
        let token_app = axum::Router::new().route(
            "/",
            axum::routing::get(move |uri: axum::http::Uri| {
                let token_calls_capture = Arc::clone(&token_calls_capture);
                let token_query_capture = Arc::clone(&token_query_capture);
                async move {
                    token_calls_capture.fetch_add(1, Ordering::SeqCst);
                    *token_query_capture.lock().unwrap() =
                        uri.query().unwrap_or_default().to_string();
                    axum::Json(serde_json::json!({
                        "Token": {
                            "Id": "isi-token-123",
                            "ExpireTime": 4_102_444_800u64
                        }
                    }))
                }
            }),
        );
        let (token_base_url, _token_handle) = spawn_upstream(token_app).await;

        #[derive(Debug, Default)]
        struct CapturedIsiRequest {
            token: Option<String>,
            query: String,
        }

        let captured_request = Arc::new(Mutex::new(CapturedIsiRequest::default()));
        let captured_request_clone = Arc::clone(&captured_request);
        let upstream_app = axum::Router::new().route(
            "/stream/v1/tts",
            axum::routing::get(
                move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                    let captured_request_clone = Arc::clone(&captured_request_clone);
                    async move {
                        let mut guard = captured_request_clone.lock().unwrap();
                        guard.token = headers
                            .get("x-nls-token")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                        guard.query = uri.query().unwrap_or_default().to_string();
                        "ok"
                    }
                },
            ),
        );
        let (upstream_base_url, _upstream_handle) = spawn_upstream(upstream_app).await;

        unsafe { std::env::set_var("_TEST_ISI_APP_KEY", "app-key-123") };
        unsafe { std::env::set_var("_TEST_ISI_ACCESS_KEY_ID", "test-id") };
        unsafe { std::env::set_var("_TEST_ISI_ACCESS_KEY_SECRET", "test-secret") };

        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: upstream_base_url.clone(),
                path_rewrites: vec![],
                auth: Some(Box::new(isi_access_token_auth_config(&token_base_url))),
            },
            ..make_api("test")
        };

        let first_uri: Uri = format!("{upstream_base_url}/stream/v1/tts?text=hello")
            .parse()
            .unwrap();
        let second_uri: Uri = format!("{upstream_base_url}/stream/v1/tts?text=hello-again")
            .parse()
            .unwrap();

        let first = forward_request(
            &api,
            Method::GET,
            &first_uri,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .await
        .unwrap();
        let second = forward_request(
            &api,
            Method::GET,
            &second_uri,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .await
        .unwrap();

        unsafe { std::env::remove_var("_TEST_ISI_APP_KEY") };
        unsafe { std::env::remove_var("_TEST_ISI_ACCESS_KEY_ID") };
        unsafe { std::env::remove_var("_TEST_ISI_ACCESS_KEY_SECRET") };

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(token_calls.load(Ordering::SeqCst), 1);

        let token_query = token_query.lock().unwrap().clone();
        assert!(token_query.contains("Action=CreateToken"));
        assert!(token_query.contains("AccessKeyId=test-id"));
        assert!(token_query.contains("SignatureMethod=HMAC-SHA1"));
        assert!(token_query.contains("Signature="));

        let captured = captured_request.lock().unwrap();
        assert_eq!(captured.token.as_deref(), Some("isi-token-123"));
        assert!(captured.query.contains("text=hello-again"));
        assert!(captured.query.contains("appkey=app-key-123"));
    }

    #[tokio::test]
    async fn forward_request_post_with_body() {
        let app = axum::Router::new().route(
            "/v1/echo",
            axum::routing::post(|body: String| async move { format!("echo: {body}") }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/echo").parse().unwrap();
        let body = Bytes::from("test payload");
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/plain".parse().unwrap());

        let result = forward_request(&api, Method::POST, &uri, &headers, body).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"echo: test payload");
    }

    #[tokio::test]
    async fn forward_request_strips_auth_header() {
        use std::sync::{Arc, Mutex};

        let received_headers: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = received_headers.clone();

        let app = axum::Router::new().route(
            "/v1/check",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let keys: Vec<String> = headers.keys().map(|k| k.to_string()).collect();
                captured.lock().unwrap().extend(keys);
                async { "ok" }
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/check").parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("x-custom", "kept".parse().unwrap());

        let result = forward_request(&api, Method::GET, &uri, &headers, Bytes::new()).await;
        assert!(result.is_ok());

        let fwd = received_headers.lock().unwrap();
        assert!(!fwd.contains(&"authorization".to_string()));
        assert!(fwd.contains(&"x-custom".to_string()));
    }

    #[tokio::test]
    async fn forward_request_preserves_status_code() {
        let app = axum::Router::new().route(
            "/v1/notfound",
            axum::routing::get(|| async { (StatusCode::NOT_FOUND, "nope") }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/notfound").parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        assert_eq!(result.unwrap().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn forward_request_upstream_down() {
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: "http://127.0.0.1:1".to_string(),
                path_rewrites: vec![],
                auth: None,
            }, // nothing listening
            ..make_api("test")
        };

        let uri: Uri = "http://127.0.0.1:1/v1/test".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        // Should return an error response (502 Bad Gateway)
        let err_resp = result.unwrap_err();
        assert_eq!(err_resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn forward_request_preserves_query_string() {
        let app = axum::Router::new().route(
            "/v1/search",
            axum::routing::get(|uri: axum::http::Uri| async move {
                uri.query().unwrap_or("none").to_string()
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/search?q=hello&limit=10")
            .parse()
            .unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let qs = String::from_utf8(body.to_vec()).unwrap();
        assert!(qs.contains("q=hello"));
        assert!(qs.contains("limit=10"));
    }

    #[tokio::test]
    async fn forward_request_with_path_rewrite() {
        use pay_types::metering::PathRewrite;

        // Upstream expects the operator's project ID in the path.
        let app = axum::Router::new().route(
            "/v3/projects/operator-proj/translate",
            axum::routing::post(|| async { "translated" }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        // SAFETY: test-only, single-threaded
        unsafe { std::env::set_var("_TEST_FWD_PROJECT", "operator-proj") };
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![PathRewrite {
                    prefix: "v3/projects/{projectId}".to_string(),
                    env: "_TEST_FWD_PROJECT".to_string(),
                }],
                auth: None,
            },
            ..make_api("test")
        };

        // Client sends their own project ID — rewrite substitutes it.
        let uri: Uri = format!("{base_url}/v3/projects/client-proj/translate")
            .parse()
            .unwrap();
        let result =
            forward_request(&api, Method::POST, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"translated");

        unsafe { std::env::remove_var("_TEST_FWD_PROJECT") };
    }

    #[tokio::test]
    async fn forward_request_injects_header_auth() {
        let auth_header: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&auth_header);
        let app = axum::Router::new().route(
            "/v1/check",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let captured = Arc::clone(&captured);
                async move {
                    *captured.lock().unwrap() = headers
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    "ok"
                }
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        // SAFETY: test-only env mutation scoped to this test.
        unsafe { std::env::set_var("_TEST_PROXY_AUTH", "secret-123") };
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url,
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Header {
                    key: "x-api-key".to_string(),
                    prefix: Some("Bearer ".to_string()),
                    value_from_env: "_TEST_PROXY_AUTH".to_string(),
                })),
            },
            ..make_api("test")
        };

        let uri: Uri = "/v1/check".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        unsafe { std::env::remove_var("_TEST_PROXY_AUTH") };

        assert_eq!(result.unwrap().status(), StatusCode::OK);
        assert_eq!(
            auth_header.lock().unwrap().as_deref(),
            Some("Bearer secret-123")
        );
    }

    #[tokio::test]
    #[serial_test::serial(alibaba_hmac_env)]
    async fn forward_request_injects_hmac_auth() {
        #[derive(Debug, Clone, Default)]
        struct CapturedRequest {
            authorization: Option<String>,
            content_md5: Option<String>,
            date: Option<String>,
            host: Option<String>,
            signature_nonce: Option<String>,
            signature_method: Option<String>,
            version: Option<String>,
        }

        let captured_headers: Arc<Mutex<CapturedRequest>> =
            Arc::new(Mutex::new(CapturedRequest::default()));
        let captured = Arc::clone(&captured_headers);
        let app = axum::Router::new().route(
            "/v1/secure",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let captured = Arc::clone(&captured);
                async move {
                    let mut guard = captured.lock().unwrap();
                    guard.authorization = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    guard.content_md5 = headers
                        .get("content-md5")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    guard.date = headers
                        .get("date")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    guard.host = headers
                        .get("host")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    guard.signature_nonce = headers
                        .get("x-acs-signature-nonce")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    guard.signature_method = headers
                        .get("x-acs-signature-method")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    guard.version = headers
                        .get("x-acs-version")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    "ok"
                }
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        unsafe { std::env::set_var("_TEST_ALIBABA_ACCESS_KEY_ID", "test-id") };
        unsafe { std::env::set_var("_TEST_ALIBABA_ACCESS_KEY_SECRET", "test-secret") };

        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: Some(Box::new(alibaba_hmac_auth_config(false))),
            },
            ..make_api("test")
        };

        let uri: Uri = "/v1/secure".parse().unwrap();
        let body =
            Bytes::from_static(br#"{"FormatType":"text","SourceLanguage":"en","TargetLanguage":"zh","SourceText":"Hello"}"#);
        let result =
            forward_request(&api, Method::POST, &uri, &HeaderMap::new(), body.clone()).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let captured = captured_headers.lock().unwrap().clone();
        let mut expected = PreparedUpstreamRequest::new(&format!("{base_url}/v1/secure")).unwrap();
        let auth = alibaba_hmac_auth_config(true);
        let AuthConfig::Hmac {
            algorithm,
            secret_from_env,
            secret_suffix,
            key_id_from_env,
            prepare,
            canonical,
            signature,
        } = &auth
        else {
            panic!("expected HMAC auth config");
        };
        expected.set_header("Date", captured.date.as_deref().unwrap());
        expected.set_header(
            "x-acs-signature-nonce",
            captured.signature_nonce.as_deref().unwrap(),
        );
        let replay_auth = AuthConfig::Hmac {
            algorithm: algorithm.clone(),
            secret_from_env: secret_from_env.clone(),
            secret_suffix: secret_suffix.clone(),
            key_id_from_env: key_id_from_env.clone(),
            prepare: prepare
                .iter()
                .map(|binding| match binding.target.name.as_str() {
                    "Date" => HmacPrepareBinding {
                        target: binding.target.clone(),
                        value: HmacPrepareValue::Literal {
                            value: captured.date.clone().unwrap(),
                        },
                    },
                    "x-acs-signature-nonce" => HmacPrepareBinding {
                        target: binding.target.clone(),
                        value: HmacPrepareValue::Literal {
                            value: captured.signature_nonce.clone().unwrap(),
                        },
                    },
                    _ => binding.clone(),
                })
                .collect(),
            canonical: canonical.clone(),
            signature: signature.clone(),
        };
        let AuthConfig::Hmac {
            algorithm,
            secret_from_env,
            secret_suffix,
            key_id_from_env,
            prepare,
            canonical,
            signature,
        } = &replay_auth
        else {
            unreachable!();
        };
        apply_hmac_auth(
            &mut expected,
            &Method::POST,
            body.as_ref(),
            &HmacAuthConfigRef {
                algorithm,
                secret_from_env,
                secret_suffix: secret_suffix.as_deref(),
                key_id_from_env: key_id_from_env.as_deref(),
                prepare,
                canonical,
                signature,
            },
        )
        .unwrap();

        unsafe { std::env::remove_var("_TEST_ALIBABA_ACCESS_KEY_ID") };
        unsafe { std::env::remove_var("_TEST_ALIBABA_ACCESS_KEY_SECRET") };

        assert_eq!(
            captured.authorization.as_deref(),
            expected.header_value("Authorization")
        );
        assert_eq!(
            captured.content_md5.as_deref(),
            expected.header_value("Content-MD5")
        );
        assert_eq!(captured.host.as_deref(), expected.header_value("Host"));
        assert_eq!(captured.signature_method.as_deref(), Some("HMAC-SHA1"));
        assert_eq!(captured.version.as_deref(), Some("2019-01-02"));
        assert!(captured.date.is_some());
        assert!(captured.signature_nonce.is_some());
    }

    #[tokio::test]
    async fn forward_request_injects_hmac_signature_into_query_param() {
        let app = axum::Router::new().route(
            "/v1/check",
            axum::routing::get(|uri: axum::http::Uri| async move {
                uri.query().unwrap_or_default().to_string()
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        unsafe { std::env::set_var("_TEST_HMAC_QUERY_SECRET", "query-secret") };
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Hmac {
                    algorithm: HmacAlgorithm::Sha256,
                    secret_from_env: "_TEST_HMAC_QUERY_SECRET".to_string(),
                    secret_suffix: None,
                    key_id_from_env: None,
                    prepare: vec![],
                    canonical: HmacCanonicalConfig {
                        join_with: "\n".to_string(),
                        components: vec![
                            HmacCanonicalComponent::Method {},
                            HmacCanonicalComponent::Path {},
                        ],
                    },
                    signature: HmacSignatureConfig {
                        encoding: HmacEncoding::Hex,
                        destination: HmacSignatureDestination {
                            kind: HmacTargetType::QueryParam,
                            name: "sig".to_string(),
                            template: "{signature}".to_string(),
                        },
                    },
                })),
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/check?existing=1").parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        unsafe { std::env::remove_var("_TEST_HMAC_QUERY_SECRET") };

        let resp = result.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let query = String::from_utf8(body.to_vec()).unwrap();
        assert!(query.contains("existing=1"));
        assert!(query.contains("sig="));
    }

    #[tokio::test]
    async fn forward_request_hmac_missing_canonical_header_returns_bad_gateway() {
        let app = axum::Router::new().route("/v1/check", axum::routing::get(|| async { "ok" }));
        let (base_url, _handle) = spawn_upstream(app).await;

        unsafe { std::env::set_var("_TEST_HMAC_MISSING_SECRET", "secret") };
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Hmac {
                    algorithm: HmacAlgorithm::Sha256,
                    secret_from_env: "_TEST_HMAC_MISSING_SECRET".to_string(),
                    secret_suffix: None,
                    key_id_from_env: None,
                    prepare: vec![],
                    canonical: HmacCanonicalConfig {
                        join_with: "\n".to_string(),
                        components: vec![HmacCanonicalComponent::Header {
                            name: "Date".to_string(),
                        }],
                    },
                    signature: HmacSignatureConfig {
                        encoding: HmacEncoding::Hex,
                        destination: HmacSignatureDestination {
                            kind: HmacTargetType::Header,
                            name: "Authorization".to_string(),
                            template: "{signature}".to_string(),
                        },
                    },
                })),
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/check").parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        unsafe { std::env::remove_var("_TEST_HMAC_MISSING_SECRET") };

        let error = result.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(error.into_body(), 2048).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["message"]
                .as_str()
                .unwrap_or_default()
                .contains("header `Date` is missing")
        );
    }

    #[tokio::test]
    async fn forward_request_injects_query_param_auth() {
        let app = axum::Router::new().route(
            "/v1/check",
            axum::routing::get(|uri: axum::http::Uri| async move {
                uri.query().unwrap_or_default().to_string()
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        // SAFETY: test-only env mutation scoped to this test.
        unsafe { std::env::set_var("_TEST_PROXY_QUERY_AUTH", "qp-secret") };
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url,
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::QueryParam {
                    key: "api_key".to_string(),
                    value_from_env: "_TEST_PROXY_QUERY_AUTH".to_string(),
                })),
            },
            ..make_api("test")
        };

        let uri: Uri = "/v1/check?existing=1".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        unsafe { std::env::remove_var("_TEST_PROXY_QUERY_AUTH") };

        let resp = result.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let query = String::from_utf8(body.to_vec()).unwrap();
        assert!(query.contains("existing=1"));
        assert!(query.contains("api_key=qp-secret"));
    }

    #[tokio::test]
    async fn forward_request_sets_content_length_for_empty_post() {
        let app = axum::Router::new().route(
            "/v1/empty",
            axum::routing::post(|headers: axum::http::HeaderMap| async move {
                headers
                    .get("content-length")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string()
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url,
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = "/v1/empty".parse().unwrap();
        let result =
            forward_request(&api, Method::POST, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"0");
    }

    #[tokio::test]
    async fn forward_request_oauth2_missing_env_returns_bad_gateway() {
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: "https://api.example.com".to_string(),
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Oauth2 {
                    token_url: "https://oauth.example.com/token".to_string(),
                    scopes: vec!["scope-a".to_string()],
                    audience: None,
                    client_id_from_env: Some("_TEST_MISSING_CLIENT_ID".to_string()),
                    client_secret_from_env: Some("_TEST_MISSING_CLIENT_SECRET".to_string()),
                    headers: HashMap::new(),
                })),
            },
            ..make_api("test")
        };

        let uri: Uri = "/v1/protected".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        let err = result.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(err.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["message"], "OAuth2 token request failed");
    }

    #[tokio::test]
    async fn forward_request_oauth2_identity_without_audience_returns_bad_gateway() {
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: "https://api.example.com".to_string(),
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Oauth2 {
                    token_url: "gcp_metadata_identity".to_string(),
                    scopes: vec![],
                    audience: None,
                    client_id_from_env: None,
                    client_secret_from_env: None,
                    headers: HashMap::new(),
                })),
            },
            ..make_api("test")
        };

        let uri: Uri = "/v1/protected".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        let err = result.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(err.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["message"], "OAuth2 token request failed");
    }

    // ── jwt_exp_claim ────────────────────────────────────────────────────

    fn make_jwt(payload: &serde_json::Value) -> String {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header = encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let body = encode(payload.to_string().as_bytes());
        format!("{header}.{body}.signature")
    }

    #[test]
    fn jwt_exp_claim_reads_exp() {
        let jwt = make_jwt(&serde_json::json!({
            "aud": "https://svc-xyz.a.run.app",
            "exp": 1_754_100_000u64,
            "iat": 1_754_096_400u64,
        }));
        assert_eq!(jwt_exp_claim(&jwt).unwrap(), 1_754_100_000);
    }

    #[test]
    fn jwt_exp_claim_rejects_non_jwt() {
        let err = jwt_exp_claim("not-a-jwt").unwrap_err();
        assert!(err.contains("3 dot-separated segments"), "got: {err}");
    }

    #[test]
    fn jwt_exp_claim_rejects_missing_exp() {
        let jwt = make_jwt(&serde_json::json!({"aud": "https://svc.example"}));
        let err = jwt_exp_claim(&jwt).unwrap_err();
        assert!(err.contains("no numeric `exp` claim"), "got: {err}");
    }

    // ── resolve_routing ──────────────────────────────────────────────────

    #[test]
    fn resolve_routing_uses_api_default() {
        let api = make_api("test");
        let r = resolve_routing(&api, "/v1/test");
        assert!(r.is_proxy());
    }

    #[test]
    fn resolve_routing_endpoint_override() {
        let mut api = make_api("test");
        api.endpoints.push(pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Post,
            path: "v1/pay".to_string(),
            description: None,
            resource: None,
            routing: Some(RoutingConfig::Respond {}),
            metering: None,
            subscription: None,
        });
        // Endpoint with override → Respond
        let r = resolve_routing(&api, "/v1/pay");
        assert!(r.is_respond());
        // Other path → falls back to API default (Proxy)
        let r2 = resolve_routing(&api, "/v1/other");
        assert!(r2.is_proxy());
    }

    #[test]
    fn resolve_routing_endpoint_no_override_uses_default() {
        let mut api = make_api("test");
        api.endpoints.push(pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Get,
            path: "v1/health".to_string(),
            description: None,
            resource: None,
            routing: None, // no override
            metering: None,
            subscription: None,
        });
        let r = resolve_routing(&api, "/v1/health");
        assert!(r.is_proxy());
    }

    #[test]
    fn resolve_routing_keeps_endpoint_override_for_browser_get_on_post_path() {
        let mut api = make_api("test");
        api.endpoints.push(pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Post,
            path: "v1/shared".to_string(),
            description: None,
            resource: None,
            routing: Some(RoutingConfig::Respond {}),
            metering: None,
            subscription: None,
        });
        api.endpoints.push(pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Get,
            path: "v1/shared".to_string(),
            description: None,
            resource: None,
            routing: None,
            metering: None,
            subscription: None,
        });

        assert!(resolve_routing(&api, "/v1/shared").is_respond());
    }

    fn endpoint_with_routing(
        path: &str,
        routing: Option<RoutingConfig>,
    ) -> pay_types::metering::Endpoint {
        pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Post,
            path: path.to_string(),
            description: None,
            resource: None,
            routing,
            metering: None,
            subscription: None,
        }
    }

    #[test]
    fn resolve_routing_applies_parameterized_endpoint_override() {
        let mut api = make_api("test");
        api.endpoints.push(endpoint_with_routing(
            "v1/projects/{projectId}/query",
            Some(RoutingConfig::Respond {}),
        ));
        api.endpoints.push(endpoint_with_routing(
            "v1/jobs/{*rest}",
            Some(RoutingConfig::Respond {}),
        ));
        api.endpoints.push(endpoint_with_routing(
            "v1beta/models/{modelId}:generate",
            Some(RoutingConfig::Respond {}),
        ));

        assert!(resolve_routing(&api, "/v1/projects/acme/query").is_respond());
        assert!(resolve_routing(&api, "/v1/jobs/42/logs").is_respond());
        assert!(resolve_routing(&api, "/v1beta/models/flash:generate").is_respond());
        // Near misses keep the API-level routing.
        assert!(resolve_routing(&api, "/v1/projects/acme/other").is_proxy());
        assert!(resolve_routing(&api, "/v1/jobs").is_proxy());
        assert!(resolve_routing(&api, "/v1beta/models/flash:stream").is_proxy());
    }

    #[test]
    fn resolve_routing_prefers_literal_endpoint_over_pattern() {
        let mut api = make_api("test");
        api.endpoints.push(endpoint_with_routing(
            "v1/projects/{projectId}/query",
            Some(RoutingConfig::Respond {}),
        ));
        api.endpoints
            .push(endpoint_with_routing("v1/projects/public/query", None));

        // The gate selects the literal endpoint, which has no override.
        assert!(resolve_routing(&api, "/v1/projects/public/query").is_proxy());
        assert!(resolve_routing(&api, "/v1/projects/acme/query").is_respond());
    }

    #[tokio::test]
    async fn forward_request_uses_parameterized_override_upstream_and_credential() {
        async fn upstream(label: &'static str) -> String {
            let app = axum::Router::new().fallback(move |req: axum::extract::Request| async move {
                let key = req
                    .headers()
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                axum::Json(serde_json::json!({"origin": label, "api_key": key}))
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            url
        }
        fn route(url: String, env: &str) -> RoutingConfig {
            RoutingConfig::Proxy {
                url,
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Header {
                    key: "x-api-key".to_string(),
                    prefix: None,
                    value_from_env: env.to_string(),
                })),
            }
        }
        unsafe {
            std::env::set_var("_TEST_ROUTE_TOP_KEY", "top-level-secret");
            std::env::set_var("_TEST_ROUTE_EP_KEY", "endpoint-secret");
        }
        let mut api = make_api("test");
        api.routing = route(upstream("top-level").await, "_TEST_ROUTE_TOP_KEY");
        api.endpoints.push(endpoint_with_routing(
            "v1/projects/{projectId}/query",
            Some(route(upstream("endpoint").await, "_TEST_ROUTE_EP_KEY")),
        ));
        api.endpoints.push(endpoint_with_routing("v1/other", None));

        let observe = |path: &'static str| {
            let api = api.clone();
            async move {
                let response = forward_request(
                    &api,
                    Method::POST,
                    &Uri::from_static(path),
                    &HeaderMap::new(),
                    Bytes::from_static(b"{}"),
                )
                .await
                .unwrap();
                let body = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()
            }
        };

        let paid = observe("/v1/projects/acme/query?sql=select%201").await;
        assert_eq!(paid["origin"], "endpoint");
        assert_eq!(paid["api_key"], "endpoint-secret");
        let other = observe("/v1/other").await;
        assert_eq!(other["origin"], "top-level");
        assert_eq!(other["api_key"], "top-level-secret");
    }

    // ── forward_request with Respond routing ─────────────────────────────

    #[tokio::test]
    async fn forward_request_respond_mode_known_endpoint() {
        let mut api = make_api("test");
        api.routing = RoutingConfig::Respond {};
        api.endpoints.push(pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Get,
            path: "v1/test".to_string(),
            description: None,
            resource: None,
            routing: None,
            metering: None,
            subscription: None,
        });

        let uri: Uri = "/v1/test".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn forward_request_respond_mode_unknown_path() {
        let mut api = make_api("test");
        api.routing = RoutingConfig::Respond {};

        let uri: Uri = "/v1/unknown".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn forward_request_respond_endpoint_override() {
        let app = axum::Router::new().route(
            "/v1/proxy-me",
            axum::routing::get(|| async { "from upstream" }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let mut api = make_api("test");
        api.routing = RoutingConfig::Proxy {
            url: base_url,
            path_rewrites: vec![],
            auth: None,
        };
        // Add an endpoint that overrides to Respond
        api.endpoints.push(pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Post,
            path: "v1/respond-only".to_string(),
            description: None,
            resource: None,
            routing: Some(RoutingConfig::Respond {}),
            metering: None,
            subscription: None,
        });
        api.endpoints.push(declared_endpoint(
            pay_types::metering::HttpMethod::Get,
            "v1/proxy-me",
        ));

        // Respond endpoint returns 200 directly
        let uri: Uri = "/v1/respond-only".parse().unwrap();
        let result =
            forward_request(&api, Method::POST, &uri, &HeaderMap::new(), Bytes::new()).await;
        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");

        // Proxy endpoint still forwards upstream
        let uri2: Uri = "/v1/proxy-me".parse().unwrap();
        let result2 =
            forward_request(&api, Method::GET, &uri2, &HeaderMap::new(), Bytes::new()).await;
        let resp2 = result2.unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
        let body2 = axum::body::to_bytes(resp2.into_body(), 1024).await.unwrap();
        assert_eq!(&body2[..], b"from upstream");
    }

    /// Upstream that redirects every request to `target`, plus a collector that
    /// records whether any request (and which credential) reached it.
    async fn spawn_redirect_and_collector(
        status: StatusCode,
    ) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let hits: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&hits);
        let collector = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
            let capture = Arc::clone(&capture);
            async move {
                capture.lock().unwrap().push(
                    headers
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string),
                );
                "collected"
            }
        });
        let (collector_url, _collector) = spawn_upstream(collector).await;
        let location = format!(
            "{}/capture",
            collector_url.replace("127.0.0.1", "localhost")
        );
        let redirect = axum::Router::new().fallback(move || {
            let location = location.clone();
            async move {
                (
                    status,
                    [(axum::http::header::LOCATION, location)],
                    "redirecting",
                )
            }
        });
        let (redirect_url, _redirect) = spawn_upstream(redirect).await;
        (redirect_url, hits)
    }

    #[tokio::test]
    async fn forward_request_does_not_follow_cross_origin_redirects() {
        for status in [
            StatusCode::MOVED_PERMANENTLY,
            StatusCode::FOUND,
            StatusCode::SEE_OTHER,
            StatusCode::TEMPORARY_REDIRECT,
            StatusCode::PERMANENT_REDIRECT,
        ] {
            let (redirect_url, hits) = spawn_redirect_and_collector(status).await;
            // SAFETY: test-only env mutation scoped to this test.
            unsafe { std::env::set_var("_TEST_PROXY_REDIRECT_KEY", "operator-secret") };
            let api = ApiSpec {
                routing: RoutingConfig::Proxy {
                    url: redirect_url,
                    path_rewrites: vec![],
                    auth: Some(Box::new(AuthConfig::Header {
                        key: "x-api-key".to_string(),
                        prefix: None,
                        value_from_env: "_TEST_PROXY_REDIRECT_KEY".to_string(),
                    })),
                },
                ..make_api("test")
            };

            let uri: Uri = "/v1/redirect".parse().unwrap();
            let resp = forward_request(
                &api,
                Method::POST,
                &uri,
                &HeaderMap::new(),
                Bytes::from_static(b"paid-protected-body"),
            )
            .await
            .unwrap();
            unsafe { std::env::remove_var("_TEST_PROXY_REDIRECT_KEY") };

            assert_eq!(resp.status(), status, "3xx must be relayed, not followed");
            assert!(resp.headers().get("location").is_some());
            assert!(
                hits.lock().unwrap().is_empty(),
                "{status}: redirect target must never receive the request"
            );
        }
    }

    #[tokio::test]
    async fn oauth2_token_fetch_does_not_follow_redirects() {
        let (redirect_url, hits) =
            spawn_redirect_and_collector(StatusCode::TEMPORARY_REDIRECT).await;
        // SAFETY: test-only env mutation scoped to this test.
        unsafe {
            std::env::set_var("_TEST_REDIRECT_OAUTH_ID", "client");
            std::env::set_var("_TEST_REDIRECT_OAUTH_SECRET", "secret");
        }
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: "http://127.0.0.1:9".to_string(),
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Oauth2 {
                    token_url: format!("{redirect_url}/token"),
                    scopes: vec![],
                    audience: None,
                    client_id_from_env: Some("_TEST_REDIRECT_OAUTH_ID".to_string()),
                    client_secret_from_env: Some("_TEST_REDIRECT_OAUTH_SECRET".to_string()),
                    headers: HashMap::new(),
                })),
            },
            ..make_api("test")
        };

        let uri: Uri = "/v1/protected".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;
        unsafe {
            std::env::remove_var("_TEST_REDIRECT_OAUTH_ID");
            std::env::remove_var("_TEST_REDIRECT_OAUTH_SECRET");
        }

        assert_eq!(result.unwrap_err().status(), StatusCode::BAD_GATEWAY);
        assert!(hits.lock().unwrap().is_empty());
    }

    async fn error_message(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        json["message"].as_str().unwrap().to_string()
    }

    fn closed_local_port() -> u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn access_token_fetch_failure_never_exposes_token_url_or_query_secret() {
        let port = closed_local_port();
        // SAFETY: test-only env mutation scoped to this test.
        unsafe { std::env::set_var("_TEST_NESTED_QUERY_SECRET", "nested-query-secret") };
        let api = api_with_auth(Some(AuthConfig::AccessToken {
            prepare: vec![],
            fetch: AccessTokenFetchConfig {
                url: format!("http://127.0.0.1:{port}/token?existing=1"),
                method: HttpMethod::Get,
                prepare: vec![],
                auth: Some(Box::new(AuthConfig::QueryParam {
                    key: "api_key".to_string(),
                    value_from_env: "_TEST_NESTED_QUERY_SECRET".to_string(),
                })),
                response: AccessTokenResponseConfig {
                    token_json_pointer: "/access_token".to_string(),
                    expires_at_json_pointer: None,
                    expires_in_json_pointer: Some("/expires_in".to_string()),
                    expires_at_format: Default::default(),
                    refresh_skew_seconds: 0,
                },
            },
            inject: AccessTokenInjectConfig {
                target: HmacTarget {
                    kind: HmacTargetType::Header,
                    name: "X-Access-Token".to_string(),
                },
                template: "{token}".to_string(),
            },
        }));

        let uri: Uri = "/v1/protected".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;
        unsafe { std::env::remove_var("_TEST_NESTED_QUERY_SECRET") };

        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let message = error_message(response).await;
        assert_eq!(message, "Access token request failed");
        assert!(!message.contains("nested-query-secret"));
        assert!(!message.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn oauth2_token_fetch_failure_never_exposes_token_url() {
        let port = closed_local_port();
        // SAFETY: test-only env mutation scoped to this test.
        unsafe {
            std::env::set_var("_TEST_FAILING_OAUTH_ID", "client");
            std::env::set_var("_TEST_FAILING_OAUTH_SECRET", "secret");
        }
        let api = api_with_auth(Some(AuthConfig::Oauth2 {
            token_url: format!("http://127.0.0.1:{port}/token?tenant=signed-value"),
            scopes: vec![],
            audience: None,
            client_id_from_env: Some("_TEST_FAILING_OAUTH_ID".to_string()),
            client_secret_from_env: Some("_TEST_FAILING_OAUTH_SECRET".to_string()),
            headers: HashMap::new(),
        }));

        let uri: Uri = "/v1/protected".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;
        unsafe {
            std::env::remove_var("_TEST_FAILING_OAUTH_ID");
            std::env::remove_var("_TEST_FAILING_OAUTH_SECRET");
        }

        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let message = error_message(response).await;
        assert_eq!(message, "OAuth2 token request failed");
        assert!(!message.contains("signed-value"));
    }

    #[tokio::test]
    async fn nested_token_request_errors_redact_query_values_for_logging() {
        let port = closed_local_port();
        let error = upstream_http_client()
            .get(format!(
                "http://127.0.0.1:{port}/token?api_key=nested-secret"
            ))
            .send()
            .await
            .unwrap_err();
        let safe = upstream_error_for_logging(&error);
        assert!(!safe.contains("nested-secret"));
        assert!(safe.contains("api_key=%5BREDACTED%5D"));
    }

    async fn spawn_secret_bound_oauth_upstream() -> (String, Arc<AtomicUsize>) {
        let token_requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&token_requests);
        let app = axum::Router::new()
            .route(
                "/token",
                axum::routing::post(
                    move |axum::extract::Form(form): axum::extract::Form<
                        HashMap<String, String>,
                    >| {
                        let counter = Arc::clone(&counter);
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            let secret = form.get("client_secret").cloned().unwrap_or_default();
                            axum::Json(serde_json::json!({
                                "access_token": format!("token-for-{secret}"),
                                "expires_in": 3600
                            }))
                        }
                    },
                ),
            )
            .fallback(|headers: axum::http::HeaderMap| async move {
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("missing")
                    .to_string()
            });
        let (base_url, _handle) = spawn_upstream(app).await;
        (base_url, token_requests)
    }

    fn secret_bound_oauth_api(base_url: &str, client_id_env: &str, secret_env: &str) -> ApiSpec {
        ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url.to_string(),
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Oauth2 {
                    token_url: format!("{base_url}/token"),
                    scopes: vec!["scope.shared".to_string()],
                    audience: None,
                    client_id_from_env: Some(client_id_env.to_string()),
                    client_secret_from_env: Some(secret_env.to_string()),
                    headers: HashMap::new(),
                })),
            },
            ..make_api("test")
        }
    }

    async fn upstream_authorization(api: &ApiSpec) -> String {
        let uri: Uri = "/upstream".parse().unwrap();
        let resp = forward_request(api, Method::GET, &uri, &HeaderMap::new(), Bytes::new())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn oauth2_cache_never_shares_tokens_across_client_secrets() {
        let (base_url, token_requests) = spawn_secret_bound_oauth_upstream().await;
        // SAFETY: test-only env mutation with names unique to this test.
        unsafe {
            std::env::set_var("_TEST_OAUTH_CACHE_SHARED_ID", "client-1");
            std::env::set_var("_TEST_OAUTH_CACHE_SECRET_A", "secret-a");
            std::env::set_var("_TEST_OAUTH_CACHE_SECRET_B", "secret-b");
        }
        let route_a = secret_bound_oauth_api(
            &base_url,
            "_TEST_OAUTH_CACHE_SHARED_ID",
            "_TEST_OAUTH_CACHE_SECRET_A",
        );
        let route_b = secret_bound_oauth_api(
            &base_url,
            "_TEST_OAUTH_CACHE_SHARED_ID",
            "_TEST_OAUTH_CACHE_SECRET_B",
        );

        assert_eq!(
            upstream_authorization(&route_a).await,
            "Bearer token-for-secret-a"
        );
        assert_eq!(
            upstream_authorization(&route_b).await,
            "Bearer token-for-secret-b"
        );
        // Positive control: each route still reuses its own cached token.
        assert_eq!(
            upstream_authorization(&route_a).await,
            "Bearer token-for-secret-a"
        );
        assert_eq!(token_requests.load(Ordering::SeqCst), 2);

        // Rotating a secret value invalidates the token minted with the old one.
        unsafe { std::env::set_var("_TEST_OAUTH_CACHE_SECRET_A", "secret-a2") };
        assert_eq!(
            upstream_authorization(&route_a).await,
            "Bearer token-for-secret-a2"
        );
        assert_eq!(token_requests.load(Ordering::SeqCst), 3);
        unsafe {
            std::env::remove_var("_TEST_OAUTH_CACHE_SHARED_ID");
            std::env::remove_var("_TEST_OAUTH_CACHE_SECRET_A");
            std::env::remove_var("_TEST_OAUTH_CACHE_SECRET_B");
        }
    }

    #[tokio::test]
    async fn forward_request_rejects_encoded_dot_segments_before_upstream() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                uri.path().to_string()
            }
        });
        let (base_url, _handle) = spawn_upstream(app).await;
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url,
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        for path in ["/v1/%2e%2e", "/v1/%2E%2E/admin", "/v1/../admin"] {
            let uri: Uri = path.parse().unwrap();
            let err = forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new())
                .await
                .unwrap_err();
            assert_eq!(err.status(), StatusCode::BAD_REQUEST, "{path}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        let uri: Uri = "/v1/item.json".parse().unwrap();
        let resp = forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"/v1/item.json");
    }

    fn declared_endpoint(
        method: pay_types::metering::HttpMethod,
        path: &str,
    ) -> pay_types::metering::Endpoint {
        pay_types::metering::Endpoint {
            method,
            path: path.to_string(),
            description: None,
            resource: None,
            routing: None,
            metering: None,
            subscription: None,
        }
    }

    #[tokio::test]
    async fn upstream_route_admits_only_the_request_it_was_minted_for() {
        let observed = Arc::new(Mutex::new(0_usize));
        let capture = Arc::clone(&observed);
        let app = axum::Router::new().fallback(move || {
            let capture = Arc::clone(&capture);
            async move {
                *capture.lock().unwrap() += 1;
                "ok"
            }
        });
        let (base_url, _handle) = spawn_upstream(app).await;
        let api = ApiSpec {
            routing: RoutingConfig::Proxy {
                url: base_url,
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };
        let other = make_api("other");
        let route = UpstreamRoute::for_tests(&api, &Method::POST, "/v1/search");

        for (api, method, path) in [
            (&api, Method::GET, "/v1/search"),
            (&api, Method::POST, "/v1/search/extra"),
            (&api, Method::POST, "/v1/other"),
            (&other, Method::POST, "/v1/search"),
        ] {
            let uri: Uri = path.parse().unwrap();
            let resp = super::forward_request(
                api,
                &route,
                method.clone(),
                &uri,
                &HeaderMap::new(),
                Bytes::new(),
            )
            .await
            .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }
        assert_eq!(*observed.lock().unwrap(), 0);

        let uri: Uri = "/v1/search".parse().unwrap();
        let resp = super::forward_request(
            &api,
            &route,
            Method::POST,
            &uri,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*observed.lock().unwrap(), 1);
    }
}
