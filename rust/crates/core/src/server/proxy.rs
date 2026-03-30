//! HTTP reverse proxy — forwards requests to upstream APIs.
//!
//! Resolves the upstream from `ApiSpec.base_url`, forwards headers and body,
//! returns the upstream response. Strips hop-by-hop and payment headers.

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use pay_types::metering::ApiSpec;
use serde_json::json;

/// Headers to strip when forwarding to upstream.
const STRIP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "transfer-encoding",
    "authorization",
    "payment-signature",
    "payment-required",
];

/// Forward a request to the upstream API defined in the spec.
///
/// - Builds the upstream URL from `api.base_url` + request path
/// - Forwards all headers except hop-by-hop and payment headers
/// - Forwards the request body as-is
/// - Returns the upstream response (status, headers, body)
pub async fn forward_request(
    api: &ApiSpec,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    let path = uri.path().trim_start_matches('/');
    let upstream_url = format!(
        "{}{}",
        api.base_url.trim_end_matches('/'),
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(path)
    );

    tracing::debug!(
        subdomain = %api.subdomain,
        upstream = %upstream_url,
        "Forwarding request"
    );

    let client = reqwest::Client::new();
    let mut upstream_req = client.request(
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap(),
        &upstream_url,
    );

    // Forward headers.
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if STRIP_HEADERS.contains(&name_str) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            upstream_req = upstream_req.header(name_str, v);
        }
    }

    // Forward body.
    if !body.is_empty() {
        upstream_req = upstream_req.body(body.to_vec());
    }

    let upstream_resp = upstream_req.send().await.map_err(|e| {
        tracing::error!(error = %e, upstream = %upstream_url, "Upstream request failed");
        error_response(StatusCode::BAD_GATEWAY, &format!("Upstream error: {e}"))
    })?;

    // Build response.
    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers() {
        if let (Ok(n), Ok(v)) = (
            axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            response_headers.insert(n, v);
        }
    }

    let response_body = upstream_resp.bytes().await.map_err(|e| {
        error_response(
            StatusCode::BAD_GATEWAY,
            &format!("Upstream body read error: {e}"),
        )
    })?;

    let mut resp = Response::builder().status(status);
    for (name, value) in &response_headers {
        resp = resp.header(name, value);
    }

    Ok(resp.body(Body::from(response_body)).unwrap())
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
