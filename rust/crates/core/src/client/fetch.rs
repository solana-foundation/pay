//! Built-in HTTP client using reqwest. No external binary needed.

use reqwest::Method;
use reqwest::blocking::Client;
use tracing::debug;

use crate::client::runner::{self, RunOutcome};
use crate::{Error, Result};

/// Fetch a URL with an explicit HTTP method/body, detecting 402 + MPP challenges.
pub fn fetch_request(
    method: &str,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<&str>,
) -> Result<RunOutcome> {
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|e| Error::Mpp(format!("Invalid HTTP method `{method}`: {e}")))?;
    fetch_request_with_method(method, url, extra_headers, body)
}

/// Fetch a URL, detecting 402 + MPP challenges.
pub fn fetch(url: &str, extra_headers: &[(String, String)]) -> Result<RunOutcome> {
    fetch_request_with_method(Method::GET, url, extra_headers, None)
}

fn fetch_request_with_method(
    method: Method,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<&str>,
) -> Result<RunOutcome> {
    let client = Client::builder()
        .user_agent(format!("pay/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create HTTP client: {e}")))?;

    debug!(%method, %url, has_body = body.is_some(), "Fetching");

    let mut req = client.request(method, url);
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

    let body = resp
        .text()
        .map_err(|e| Error::Mpp(format!("Failed to read body: {e}")))?;

    debug!(status, "Fetch complete");

    if status == 402 {
        return Ok(runner::classify_402(&headers, Some(&body), url));
    }

    let exit_code = if status >= 400 { 1 } else { 0 };
    Ok(RunOutcome::Completed {
        exit_code,
        body: Some(body),
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
            RunOutcome::Completed { exit_code, body } => {
                assert_eq!(exit_code, 0);
                assert_eq!(body.unwrap(), "hello world");
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
            RunOutcome::Completed { exit_code, body } => {
                assert_eq!(exit_code, 1);
                assert_eq!(body.unwrap(), "not found");
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
                assert_eq!(body.unwrap(), "test-value");
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
            RunOutcome::Completed { exit_code, body } => {
                assert_eq!(exit_code, 0);
                assert_eq!(body.unwrap(), "{\"query\":\"SELECT 1\"}");
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
}
