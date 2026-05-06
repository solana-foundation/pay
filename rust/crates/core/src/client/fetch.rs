//! Built-in HTTP client using reqwest. No external binary needed.

use reqwest::Method;
use reqwest::blocking::Client;
use tracing::debug;

use crate::client::runner::{self, RunOutcome};
use crate::{Error, Result};

/// Raw HTTP response — keeps status, headers, and body together so callers
/// (e.g. the rich probe pipeline) can both run `classify_402` and extract
/// additional 402 metadata without a second request.
///
/// `body` is held as raw bytes so binary responses (images, PDFs,
/// arbitrary `application/octet-stream`) round-trip without UTF-8
/// mangling. Use [`RawResponse::body_text`] when a string view is needed
/// (e.g. parsing a 402 challenge body, which is always JSON).
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    /// UTF-8 view of the body, with invalid sequences replaced by `U+FFFD`.
    /// Right call for text-typed responses (`text/*`, `application/json`,
    /// `application/xml`) where the wire format is guaranteed to be UTF-8.
    /// Wrong call for `image/*`, `application/pdf`, etc. — those should be
    /// handled as `Vec<u8>` directly.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// `content-type` header value (case-insensitive lookup), or `None` if
    /// the server didn't send one. Includes the full value with
    /// parameters (e.g. `text/plain; charset=utf-8`); use
    /// [`RawResponse::mime_type`] to strip params and lowercase.
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
    }

    /// Lowercased MIME type with parameters stripped — `"text/plain"` from
    /// `"Text/Plain; charset=UTF-8"`. Empty string when the header is
    /// missing or malformed.
    pub fn mime_type(&self) -> String {
        self.content_type()
            .and_then(|v| v.split(';').next())
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default()
    }
}

/// Fetch a URL with an explicit HTTP method/body, detecting 402 + MPP challenges.
pub fn fetch_request(
    method: &str,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<&str>,
) -> Result<RunOutcome> {
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|e| Error::Mpp(format!("Invalid HTTP method `{method}`: {e}")))?;
    let raw = fetch_raw_with_method(method, url, extra_headers, body)?;
    Ok(raw_to_outcome(raw, url))
}

/// Fetch a URL, detecting 402 + MPP challenges.
pub fn fetch(url: &str, extra_headers: &[(String, String)]) -> Result<RunOutcome> {
    let raw = fetch_raw_with_method(Method::GET, url, extra_headers, None)?;
    Ok(raw_to_outcome(raw, url))
}

/// Fetch a URL and return the raw status/headers/body — no 402 classification.
///
/// Use this when the caller needs to do its own analysis of the response
/// (e.g. the skills probe enriches the response with all advertised payment
/// protocols, not just the one Pay would settle on).
pub fn fetch_raw(
    method: &str,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<&str>,
) -> Result<RawResponse> {
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|e| Error::Mpp(format!("Invalid HTTP method `{method}`: {e}")))?;
    fetch_raw_with_method(method, url, extra_headers, body)
}

fn raw_to_outcome(raw: RawResponse, url: &str) -> RunOutcome {
    if raw.status == 402 {
        // 402 challenge bodies are always JSON-as-text per spec; the
        // text view is correct here.
        return runner::classify_402(&raw.headers, Some(&raw.body_text()), url);
    }
    let exit_code = if raw.status >= 400 { 1 } else { 0 };
    let content_type = raw.content_type().map(str::to_string);
    RunOutcome::Completed {
        exit_code,
        body: Some(raw.body),
        content_type,
    }
}

fn fetch_raw_with_method(
    method: Method,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<&str>,
) -> Result<RawResponse> {
    let client = Client::builder()
        .user_agent(format!("pay/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create HTTP client: {e}")))?;

    // When the debugger proxy is active, route through it so PDB captures
    // the traffic. The original URL is passed in X-Pay-Forward-To.
    let (actual_url, forward_header) = if let Ok(proxy) = std::env::var("PAY_DEBUGGER_PROXY") {
        // Rewrite: https://gateway/path → http://127.0.0.1:1402/path
        let path = url
            .find("://")
            .and_then(|i| url[i + 3..].find('/'))
            .map(|i| &url[url.find("://").unwrap() + 3 + i..])
            .unwrap_or("/");
        let proxy_url = format!("{}{}", proxy.trim_end_matches('/'), path);
        debug!(%url, %proxy_url, "Routing through debugger proxy");
        (proxy_url, Some(url.to_string()))
    } else {
        (url.to_string(), None)
    };

    debug!(%method, url = %actual_url, has_body = body.is_some(), "Fetching");

    let mut req = client.request(method, &actual_url);
    if let Some(dest) = &forward_header {
        req = req.header("x-pay-forward-to", dest.as_str());
    }
    for (key, value) in extra_headers {
        req = req.header(key.as_str(), value.as_str());
    }
    if let Some(body) = body {
        req = req.body(body.to_owned());
    }

    let resp = req
        .send()
        .map_err(|e| Error::Mpp(format!("Request failed: {e}")))?;
    let status = resp.status().as_u16();

    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_lowercase(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();

    // Use `bytes()` not `text()` — `text()` UTF-8-decodes lossily and
    // replaces non-UTF-8 sequences with `U+FFFD`, irreversibly mangling
    // binary responses (images, PDFs, octet-streams). Callers that want
    // a string view ask for `body_text()` explicitly.
    let body = resp
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| Error::Mpp(format!("Failed to read body: {e}")))?;

    debug!(status, "Fetch complete");

    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::client::runner::RunOutcome;

    /// Start a background server on a random port, return its URL.
    /// Uses a separate thread with its own tokio runtime to avoid
    /// conflicts with reqwest::blocking inside fetch().
    fn start_server(handler: axum::Router) -> String {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tx.send(format!("http://{addr}")).unwrap();
                axum::serve(listener, handler).await.ok();
            });
        });
        let url = rx.recv().unwrap();
        // Give the server time to accept connections
        std::thread::sleep(std::time::Duration::from_millis(50));
        url
    }

    #[test]
    fn fetch_200_returns_body() {
        let app =
            axum::Router::new().route("/data", axum::routing::get(|| async { "hello world" }));
        let base_url = start_server(app);

        let result = fetch(&format!("{base_url}/data"), &[]).unwrap();
        match result {
            RunOutcome::Completed {
                exit_code, body, ..
            } => {
                assert_eq!(exit_code, 0);
                assert_eq!(body.unwrap(), b"hello world");
            }
            _ => panic!("Expected Completed"),
        }
    }

    #[test]
    fn fetch_404_returns_exit_code_1() {
        let app = axum::Router::new().route(
            "/missing",
            axum::routing::get(|| async { (axum::http::StatusCode::NOT_FOUND, "not found") }),
        );
        let base_url = start_server(app);

        let result = fetch(&format!("{base_url}/missing"), &[]).unwrap();
        match result {
            RunOutcome::Completed {
                exit_code, body, ..
            } => {
                assert_eq!(exit_code, 1);
                assert_eq!(body.unwrap(), b"not found");
            }
            _ => panic!("Expected Completed"),
        }
    }

    #[test]
    fn fetch_402_without_mpp_returns_unknown() {
        let app = axum::Router::new().route(
            "/paid",
            axum::routing::get(|| async { (axum::http::StatusCode::PAYMENT_REQUIRED, "pay up") }),
        );
        let base_url = start_server(app);

        let result = fetch(&format!("{base_url}/paid"), &[]).unwrap();
        assert!(matches!(result, RunOutcome::UnknownPaymentRequired { .. }));
    }

    #[test]
    fn fetch_sends_extra_headers() {
        let app = axum::Router::new().route(
            "/echo-header",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                headers
                    .get("x-custom")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("missing")
                    .to_string()
            }),
        );
        let base_url = start_server(app);

        let headers = vec![("x-custom".to_string(), "test-value".to_string())];
        let result = fetch(&format!("{base_url}/echo-header"), &headers).unwrap();
        match result {
            RunOutcome::Completed { body, .. } => {
                assert_eq!(body.unwrap(), b"test-value");
            }
            _ => panic!("Expected Completed"),
        }
    }

    #[test]
    fn fetch_request_sends_post_body() {
        let app = axum::Router::new().route(
            "/echo-body",
            axum::routing::post(|body: String| async move { body }),
        );
        let base_url = start_server(app);

        let result = fetch_request(
            "POST",
            &format!("{base_url}/echo-body"),
            &[("content-type".to_string(), "application/json".to_string())],
            Some("{\"query\":\"SELECT 1\"}"),
        )
        .unwrap();

        match result {
            RunOutcome::Completed {
                exit_code, body, ..
            } => {
                assert_eq!(exit_code, 0);
                assert_eq!(body.unwrap(), br#"{"query":"SELECT 1"}"#);
            }
            _ => panic!("Expected Completed"),
        }
    }

    #[test]
    fn fetch_request_rejects_invalid_method() {
        let result = fetch_request("BAD METHOD", "https://example.com", &[], None);
        assert!(result.is_err());
    }

    #[test]
    fn fetch_invalid_url_errors() {
        let result = fetch("not-a-url", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn fetch_connection_refused_errors() {
        let result = fetch("http://127.0.0.1:1/nope", &[]);
        assert!(result.is_err());
    }

    /// Regression for #350.4: binary responses must round-trip byte-for-byte
    /// — `text()` UTF-8 decoding silently mangles non-UTF-8 sequences (PNG
    /// header `0x89 0x50 0x4E 0x47` becomes `U+FFFD 0x50 0x4E 0x47`), which
    /// is an irreversible corruption.
    #[test]
    fn fetch_preserves_binary_bytes() {
        let payload: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0xFE, 0x00, 0x01,
        ];
        let payload_for_handler = payload.clone();
        let app = axum::Router::new().route(
            "/blob",
            axum::routing::get(move || {
                let bytes = payload_for_handler.clone();
                async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                        bytes,
                    )
                }
            }),
        );
        let base_url = start_server(app);

        let raw = fetch_raw("GET", &format!("{base_url}/blob"), &[], None).unwrap();
        assert_eq!(raw.body, payload, "raw bytes must match exactly");
        assert_eq!(raw.mime_type(), "application/octet-stream");
    }

    #[test]
    fn fetch_completed_carries_content_type() {
        let app = axum::Router::new().route(
            "/img",
            axum::routing::get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "image/png")],
                    vec![0x89, b'P', b'N', b'G'],
                )
            }),
        );
        let base_url = start_server(app);

        let result = fetch(&format!("{base_url}/img"), &[]).unwrap();
        match result {
            RunOutcome::Completed {
                content_type, body, ..
            } => {
                assert_eq!(content_type.as_deref(), Some("image/png"));
                assert_eq!(body.unwrap(), vec![0x89, b'P', b'N', b'G']);
            }
            _ => panic!("Expected Completed"),
        }
    }

    #[test]
    fn body_text_replaces_invalid_utf8() {
        let raw = RawResponse {
            status: 200,
            headers: vec![],
            body: vec![0xFF, 0xFE, b'h', b'i'],
        };
        let text = raw.body_text();
        assert!(text.contains("hi"));
        assert!(text.contains('\u{FFFD}'));
    }

    #[test]
    fn content_type_lookup_is_case_insensitive() {
        let raw = RawResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "image/jpeg".to_string())],
            body: vec![],
        };
        assert_eq!(raw.content_type(), Some("image/jpeg"));
        assert_eq!(raw.mime_type(), "image/jpeg");
    }

    #[test]
    fn mime_type_strips_parameters() {
        let raw = RawResponse {
            status: 200,
            headers: vec![(
                "content-type".to_string(),
                "Text/Plain; charset=UTF-8".to_string(),
            )],
            body: vec![],
        };
        assert_eq!(raw.mime_type(), "text/plain");
    }

    #[test]
    fn mime_type_empty_when_header_missing() {
        let raw = RawResponse {
            status: 200,
            headers: vec![],
            body: vec![],
        };
        assert_eq!(raw.mime_type(), "");
    }
}
