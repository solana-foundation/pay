//! Request logging middleware — captures request/response data and feeds the correlation engine.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::Extension;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;

use crate::PdbState;
use crate::types::LogEntry;

/// Axum middleware that logs every proxied request/response into the correlation engine.
///
/// Must be the outermost layer so it sees the full lifecycle including 402 challenges.
/// Skips `/__402/` paths.
///
/// Uses `Extension<Option<PdbState>>` — no-op when debugger is disabled.
pub async fn logging_middleware(
    Extension(pdb): Extension<Option<PdbState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let Some(pdb) = pdb else {
        return next.run(req).await;
    };

    let path = req.uri().path().to_string();

    // Skip internal paths
    if path.starts_with("/__402") {
        return next.run(req).await;
    }

    let method = req.method().to_string();
    let req_headers = extract_headers(req.headers());
    let client_ip = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| {
            req.headers()
                .get("host")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".into());

    let start = std::time::Instant::now();

    let response = next.run(req).await;

    let status = response.status().as_u16();
    let res_headers = extract_headers(response.headers());

    // Consume body to capture it, then re-wrap.
    // Use 256KB limit to handle HTML payment pages (~50KB).
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 256 * 1024)
        .await
        .unwrap_or_default();

    let res_body = if bytes.is_empty() {
        None
    } else {
        let s = String::from_utf8_lossy(&bytes);
        Some(if s.len() > 4096 {
            format!("{}…", &s[..4096])
        } else {
            s.to_string()
        })
    };

    let ms = start.elapsed().as_millis() as u64;

    let entry = LogEntry {
        id: pdb.next_log_id(),
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        method,
        path,
        status,
        ms,
        req_headers,
        res_headers,
        res_body,
        client_ip,
    };

    pdb.correlation.lock().unwrap().ingest(entry);

    Response::from_parts(parts, Body::from(bytes))
}

fn extract_headers(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect()
}
