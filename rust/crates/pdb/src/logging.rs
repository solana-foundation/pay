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
    if is_streaming_response(&res_headers) {
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
            res_body: Some("<streaming response; body not captured>".to_string()),
            client_ip,
        };

        pdb.correlation.lock().unwrap().ingest(entry);
        return response;
    }

    // Capture body for debugger (small enough to be useful, large enough to
    // cover OpenAPI specs etc.). Once `to_bytes` errors on size overflow the
    // body is consumed and unrecoverable, so we pre-check Content-Length and
    // pass oversized responses through *without* capturing them — the body
    // stays intact for the client.
    const BODY_CAPTURE_LIMIT: usize = 8 * 1024 * 1024;

    let advertised_len = response
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());

    let (response, res_body): (Response, Option<String>) = match advertised_len {
        Some(len) if len > BODY_CAPTURE_LIMIT => (
            response,
            Some(format!(
                "<elided: response body {len} bytes exceeds {BODY_CAPTURE_LIMIT} capture limit>"
            )),
        ),
        _ => {
            let (parts, body) = response.into_parts();
            match axum::body::to_bytes(body, BODY_CAPTURE_LIMIT).await {
                Ok(bytes) => {
                    let summary = if bytes.is_empty() {
                        None
                    } else {
                        let s = String::from_utf8_lossy(&bytes);
                        Some(if s.len() > 4096 {
                            format!("{}…", &s[..4096])
                        } else {
                            s.to_string()
                        })
                    };
                    (Response::from_parts(parts, Body::from(bytes)), summary)
                }
                // Body unbounded and too big — body is now consumed, we can
                // only return an error response. This is the rare case where
                // a streamed response without Content-Length blew past the
                // limit; debugger UX trumps client UX here since we can't
                // recover the bytes.
                Err(_) => (
                    Response::from_parts(parts, Body::empty()),
                    Some(format!(
                        "<elided: streaming response exceeded {BODY_CAPTURE_LIMIT} byte capture limit>"
                    )),
                ),
            }
        }
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

    response
}

fn is_streaming_response(headers: &HashMap<String, String>) -> bool {
    headers
        .get("content-type")
        .map(|content_type| {
            let content_type = content_type.to_ascii_lowercase();
            content_type.starts_with("text/event-stream")
                || content_type.starts_with("application/x-ndjson")
        })
        .unwrap_or(false)
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
