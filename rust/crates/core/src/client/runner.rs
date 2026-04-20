use std::process::{Command, Stdio};

use tempfile::NamedTempFile;
use tracing::{debug, info};

use solana_x402::protocol::methods::solana::PaymentRequirements;

use crate::client::mpp;
use crate::client::x402;
use crate::{Error, Result};

/// The outcome of running a wrapped command.
#[derive(Debug)]
pub enum RunOutcome {
    /// The server returned 402 with an MPP charge challenge.
    MppChallenge {
        challenge: Box<mpp::Challenge>,
        resource_url: String,
    },
    /// The server returned 402 with an MPP session challenge (intent="session").
    /// Session payments require a stateful client with a Fiber channel.
    SessionChallenge {
        challenge: Box<mpp::Challenge>,
        resource_url: String,
    },
    /// The server returned 402 with an x402 challenge.
    X402Challenge {
        requirements: Box<PaymentRequirements>,
        resource_url: String,
    },
    /// The server returned 402 but without a recognized payment protocol.
    UnknownPaymentRequired {
        headers: Vec<(String, String)>,
        resource_url: String,
    },
    /// The server returned 402 with a `verification_failed` body — this is a
    /// retry response telling the client *why* the previously-submitted payment
    /// was rejected (wrong network, expired, double-spend, etc.).
    PaymentRejected {
        reason: String,
        retryable: bool,
        resource_url: String,
    },
    /// The command completed (any status other than 402).
    Completed {
        exit_code: i32,
        /// Response body (only set by the built-in fetch, not by curl/wget wrappers).
        body: Option<String>,
    },
}

/// Run `curl` with the given user args, detecting 402 + MPP challenges.
///
/// Appends `-D <tempfile>` after user args to capture response headers.
/// stdout/stderr/stdin are inherited so the user sees normal curl output.
pub fn run_curl(user_args: &[String]) -> Result<RunOutcome> {
    run_curl_inner(user_args, &[])
}

/// Run `curl` with extra headers injected (used for retry after payment).
pub fn run_curl_with_headers(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    run_curl_inner(user_args, extra_headers)
}

fn run_curl_inner(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    check_command_exists("curl")?;

    let header_file = NamedTempFile::new()?;
    let header_path = header_file.path();
    let body_file = NamedTempFile::new()?;
    let body_path = body_file.path();

    debug!(args = ?user_args, extra = ?extra_headers, "Running curl");

    // Capture body to file. Capture stderr so we can swallow it on 402.
    let mut cmd = Command::new("curl");
    cmd.args(user_args);
    for h in extra_headers {
        cmd.arg("-H").arg(h);
    }
    cmd.arg("-D").arg(header_path);
    cmd.arg("-o").arg(body_path);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let output = cmd.output()?;
    let exit_code = output.status.code().unwrap_or(1);
    let headers_raw = std::fs::read_to_string(header_path).unwrap_or_default();
    let body = std::fs::read_to_string(body_path).unwrap_or_default();
    let (status_code, headers) = parse_http_headers(&headers_raw);
    let url = find_url_in_args(user_args).unwrap_or_default();

    debug!(?status_code, exit_code, "curl finished");

    if status_code == Some(402) {
        // Swallow stderr and body on 402 — CLI handles display
        return Ok(classify_402(&headers, Some(&body), &url));
    }

    // Not 402 — re-emit stderr (progress bar etc.) and print body
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    print!("{body}");
    Ok(RunOutcome::Completed {
        exit_code,
        body: None,
    })
}

/// Run `wget` with the given user args, detecting 402 + MPP challenges.
pub fn run_wget(user_args: &[String]) -> Result<RunOutcome> {
    run_wget_inner(user_args, &[])
}

/// Run `wget` with extra headers injected (used for retry after payment).
pub fn run_wget_with_headers(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    run_wget_inner(user_args, extra_headers)
}

fn run_wget_inner(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    check_command_exists("wget")?;

    let has_server_response = user_args
        .iter()
        .any(|a| a == "-S" || a == "--server-response");

    let mut cmd = Command::new("wget");
    if !has_server_response {
        cmd.arg("--server-response");
    }
    cmd.args(user_args);
    for h in extra_headers {
        cmd.arg("--header").arg(h);
    }
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::piped());

    debug!(args = ?user_args, extra = ?extra_headers, "Running wget");

    let output = cmd.output()?;
    let exit_code = output.status.code().unwrap_or(1);
    let stderr_text = String::from_utf8_lossy(&output.stderr);

    let (status_code, headers) = parse_wget_headers(&stderr_text);
    let url = find_url_in_args(user_args).unwrap_or_default();

    debug!(?status_code, exit_code, "wget finished");

    if status_code == Some(402) {
        // Swallow stderr on 402. NOTE: wget writes the body to a file in cwd
        // by default, which we don't want to clobber by injecting -O. As a
        // result, we can't surface server `verification_failed` reasons for
        // wget retries (only curl/fetch). The retry path falls back to a
        // generic "still 402" message.
        return Ok(classify_402(&headers, None, &url));
    }

    // Re-emit stderr on success
    eprint!("{stderr_text}");
    Ok(RunOutcome::Completed {
        exit_code,
        body: None,
    })
}

/// Given 402 headers (and optional body), determine the payment protocol.
pub(crate) fn classify_402(
    headers: &[(String, String)],
    body: Option<&str>,
    resource_url: &str,
) -> RunOutcome {
    // A `verification_failed` body wins over a fresh challenge: it means the
    // server saw our payment header and rejected it. We must surface the
    // reason instead of looping into a second pay-and-retry.
    if let Some((reason, retryable)) = parse_verification_failure(body) {
        info!(resource = resource_url, %reason, "Server rejected payment");
        return RunOutcome::PaymentRejected {
            reason,
            retryable,
            resource_url: resource_url.to_string(),
        };
    }

    // Check for MPP challenge in www-authenticate header
    if let Some(www_auth) = headers.iter().find(|(k, _)| k == "www-authenticate")
        && let Some(challenge) = mpp::parse(&www_auth.1)
    {
        if challenge.intent.as_str() == "session" {
            info!(resource = resource_url, "Detected MPP session challenge");
            return RunOutcome::SessionChallenge {
                challenge: Box::new(challenge),
                resource_url: resource_url.to_string(),
            };
        }
        info!(resource = resource_url, "Detected MPP challenge");
        return RunOutcome::MppChallenge {
            challenge: Box::new(challenge),
            resource_url: resource_url.to_string(),
        };
    }

    // Check for x402 challenge (header or body)
    if let Some(requirements) = x402::parse(headers, body) {
        info!(resource = resource_url, "Detected x402 challenge");
        return RunOutcome::X402Challenge {
            requirements: Box::new(requirements),
            resource_url: resource_url.to_string(),
        };
    }

    RunOutcome::UnknownPaymentRequired {
        headers: headers.to_vec(),
        resource_url: resource_url.to_string(),
    }
}

/// Pure parser: pulls a `verification_failed` reason out of a 402 JSON body.
///
/// Returns `(message, retryable)` if the body matches the shape emitted by
/// `crates/core/src/server/payment.rs` for verification failures:
///
/// ```json
/// {"error": "verification_failed", "message": "...", "retryable": false}
/// ```
///
/// Returns `None` for any other body shape (or absent body), so the caller
/// can fall through to the normal challenge-detection path.
pub(crate) fn parse_verification_failure(body: Option<&str>) -> Option<(String, bool)> {
    let body = body?.trim();
    if body.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("error")?.as_str()? != "verification_failed" {
        return None;
    }
    let message = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("payment verification failed")
        .to_string();
    let retryable = v
        .get("retryable")
        .and_then(|r| r.as_bool())
        .unwrap_or(false);
    Some((message, retryable))
}

fn check_command_exists(cmd: &str) -> Result<()> {
    match Command::new("which").arg(cmd).output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err(Error::CommandNotFound {
            cmd: cmd.to_string(),
        }),
    }
}

/// Parse HTTP headers from curl's `-D` dump format.
///
/// Handles redirect chains by taking the LAST header block (the final response).
fn parse_http_headers(raw: &str) -> (Option<u16>, Vec<(String, String)>) {
    let blocks: Vec<&str> = raw.split("\r\n\r\n").filter(|b| !b.is_empty()).collect();
    let block = match blocks.last() {
        Some(b) => b,
        None => return (None, vec![]),
    };

    let mut status_code = None;
    let mut headers = Vec::new();

    for line in block.lines() {
        let line = line.trim();
        if line.starts_with("HTTP/") {
            status_code = line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok());
        } else if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_lowercase(), value.trim().to_string()));
        }
    }

    (status_code, headers)
}

/// Parse HTTP headers from wget's `--server-response` stderr output.
fn parse_wget_headers(stderr: &str) -> (Option<u16>, Vec<(String, String)>) {
    let mut status_code = None;
    let mut headers = Vec::new();

    let mut current_status = None;
    let mut current_headers = Vec::new();

    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("HTTP/") {
            if current_status.is_some() {
                status_code = current_status;
                headers = std::mem::take(&mut current_headers);
            }
            current_status = trimmed
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok());
        } else if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            if !key.is_empty() && !key.contains(' ') {
                current_headers.push((key.to_lowercase(), value.trim().to_string()));
            }
        }
    }

    if current_status.is_some() {
        status_code = current_status;
        headers = current_headers;
    }

    (status_code, headers)
}

/// Heuristic: find the URL from command args.
fn find_url_in_args(args: &[String]) -> Option<String> {
    args.iter()
        .find(|a| a.starts_with("http://") || a.starts_with("https://"))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_headers() {
        let raw = "HTTP/1.1 402 Payment Required\r\nX-Payment-Url: https://pay.example.com\r\nX-Payment-Amount: 1000\r\nX-Payment-Currency: USD\r\n\r\n";
        let (status, headers) = parse_http_headers(raw);
        assert_eq!(status, Some(402));
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-payment-url")
                .unwrap()
                .1,
            "https://pay.example.com"
        );
    }

    #[test]
    fn parse_redirect_chain_takes_last() {
        let raw = "HTTP/1.1 301 Moved\r\nLocation: https://new.example.com\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n";
        let (status, _headers) = parse_http_headers(raw);
        assert_eq!(status, Some(200));
    }

    #[test]
    fn parse_wget_server_response() {
        let stderr = r#"
--2026-03-20 10:00:00--  https://example.com/resource
Resolving example.com... 93.184.216.34
Connecting to example.com|93.184.216.34|:443... connected.
HTTP request sent, awaiting response...
  HTTP/1.1 402 Payment Required
  X-Payment-Url: https://pay.example.com
  X-Payment-Amount: 500
  X-Payment-Currency: SOL
  Content-Length: 0
"#;
        let (status, headers) = parse_wget_headers(stderr);
        assert_eq!(status, Some(402));
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-payment-url")
                .unwrap()
                .1,
            "https://pay.example.com"
        );
    }

    #[test]
    fn classify_402_with_mpp() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let request_json = serde_json::json!({
            "amount": "1000000",
            "currency": "USDC",
            "recipient": "So11111111111111111111111111111111111111112",
            "methodDetails": {
                "network": "devnet"
            }
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request_json).unwrap());
        let headers = vec![(
            "www-authenticate".to_string(),
            format!(
                "Payment id=\"test-id\", realm=\"test\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
            ),
        )];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(matches!(outcome, RunOutcome::MppChallenge { .. }));
    }

    #[test]
    fn classify_402_with_session_mpp() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let request_json = serde_json::json!({
            "cap": "1000000",
            "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "network": "localnet",
            "operator": "So11111111111111111111111111111111111111112",
            "recipient": "So11111111111111111111111111111111111111112",
            "modes": ["pull"]
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request_json).unwrap());
        let headers = vec![(
            "www-authenticate".to_string(),
            format!(
                "Payment id=\"test-id\", realm=\"test\", method=\"solana\", intent=\"session\", request=\"{b64}\""
            ),
        )];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(matches!(outcome, RunOutcome::SessionChallenge { .. }));
    }

    #[test]
    fn classify_402_with_x402_header() {
        let requirements = serde_json::json!({
            "network": "solana",
            "cluster": "devnet",
            "recipient": "So11111111111111111111111111111111111111112",
            "amount": "1000000",
            "currency": "USDC",
            "resource": "https://example.com/resource"
        });
        let headers = vec![("x-payment-required".to_string(), requirements.to_string())];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(matches!(outcome, RunOutcome::X402Challenge { .. }));
    }

    #[test]
    fn classify_402_without_mpp() {
        let headers = vec![("content-type".to_string(), "text/html".to_string())];
        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(matches!(outcome, RunOutcome::UnknownPaymentRequired { .. }));
    }

    // ── parse_verification_failure ──────────────────────────────────────────

    #[test]
    fn parse_verification_failure_full_payload() {
        let body = r#"{"error":"verification_failed","message":"transaction not found on devnet","retryable":false}"#;
        let parsed = parse_verification_failure(Some(body));
        assert_eq!(
            parsed,
            Some(("transaction not found on devnet".to_string(), false))
        );
    }

    #[test]
    fn parse_verification_failure_retryable_true() {
        let body = r#"{"error":"verification_failed","message":"rpc temporarily unavailable","retryable":true}"#;
        let parsed = parse_verification_failure(Some(body));
        assert_eq!(
            parsed,
            Some(("rpc temporarily unavailable".to_string(), true))
        );
    }

    #[test]
    fn parse_verification_failure_missing_message_uses_default() {
        let body = r#"{"error":"verification_failed","retryable":false}"#;
        let parsed = parse_verification_failure(Some(body));
        assert_eq!(
            parsed,
            Some(("payment verification failed".to_string(), false))
        );
    }

    #[test]
    fn parse_verification_failure_missing_retryable_defaults_false() {
        let body = r#"{"error":"verification_failed","message":"bad signature"}"#;
        let parsed = parse_verification_failure(Some(body));
        assert_eq!(parsed, Some(("bad signature".to_string(), false)));
    }

    #[test]
    fn parse_verification_failure_wrong_error_field() {
        // First-call 402 challenge body — must NOT be treated as a rejection.
        let body = r#"{"error":"payment_required","message":"This endpoint requires payment."}"#;
        assert_eq!(parse_verification_failure(Some(body)), None);
    }

    #[test]
    fn parse_verification_failure_not_json() {
        assert_eq!(parse_verification_failure(Some("not json at all")), None);
    }

    #[test]
    fn parse_verification_failure_empty_string() {
        assert_eq!(parse_verification_failure(Some("")), None);
        assert_eq!(parse_verification_failure(Some("   ")), None);
    }

    #[test]
    fn parse_verification_failure_none() {
        assert_eq!(parse_verification_failure(None), None);
    }

    #[test]
    fn classify_402_verification_failed_wins_over_challenge() {
        // Even if a fresh www-authenticate challenge is present, a
        // verification_failed body must take precedence — otherwise the
        // client would loop into a second pay-and-retry instead of
        // surfacing why the first payment was rejected.
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let request_json = serde_json::json!({
            "amount": "1000000",
            "currency": "USDC",
            "recipient": "So11111111111111111111111111111111111111112",
            "methodDetails": { "network": "devnet" }
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request_json).unwrap());
        let headers = vec![(
            "www-authenticate".to_string(),
            format!(
                "Payment id=\"test-id\", realm=\"test\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
            ),
        )];
        let body = r#"{"error":"verification_failed","message":"wrong network: expected localnet","retryable":false}"#;

        let outcome = classify_402(&headers, Some(body), "https://example.com/resource");
        match outcome {
            RunOutcome::PaymentRejected {
                reason, retryable, ..
            } => {
                assert_eq!(reason, "wrong network: expected localnet");
                assert!(!retryable);
            }
            other => panic!("expected PaymentRejected, got {other:?}"),
        }
    }

    #[test]
    fn classify_402_unrelated_body_falls_through_to_challenge() {
        // First-call 402 with a JSON body that isn't verification_failed —
        // we still detect the MPP challenge from headers.
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let request_json = serde_json::json!({
            "amount": "1000000",
            "currency": "USDC",
            "recipient": "So11111111111111111111111111111111111111112",
            "methodDetails": { "network": "devnet" }
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request_json).unwrap());
        let headers = vec![(
            "www-authenticate".to_string(),
            format!(
                "Payment id=\"test-id\", realm=\"test\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
            ),
        )];
        let body = r#"{"error":"payment_required","message":"This endpoint requires payment."}"#;

        let outcome = classify_402(&headers, Some(body), "https://example.com/resource");
        assert!(matches!(outcome, RunOutcome::MppChallenge { .. }));
    }

    #[test]
    fn find_url_from_args() {
        let args: Vec<String> = vec![
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "https://example.com/api",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            find_url_in_args(&args),
            Some("https://example.com/api".to_string())
        );
    }

    #[test]
    fn find_url_none_when_missing() {
        let args: Vec<String> = vec!["-v", "--compressed"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(find_url_in_args(&args), None);
    }

    #[test]
    fn find_url_http() {
        let args = vec!["http://localhost:8080/test".to_string()];
        assert_eq!(
            find_url_in_args(&args),
            Some("http://localhost:8080/test".to_string())
        );
    }

    #[test]
    fn find_url_returns_first_url_when_multiple_present() {
        let args = vec![
            "https://first.example.com".to_string(),
            "https://second.example.com".to_string(),
        ];
        assert_eq!(
            find_url_in_args(&args),
            Some("https://first.example.com".to_string())
        );
    }

    #[test]
    fn parse_empty_headers() {
        let (status, headers) = parse_http_headers("");
        assert_eq!(status, None);
        assert!(headers.is_empty());
    }

    #[test]
    fn parse_status_only() {
        let raw = "HTTP/1.1 200 OK\r\n\r\n";
        let (status, headers) = parse_http_headers(raw);
        assert_eq!(status, Some(200));
        assert!(headers.is_empty());
    }

    #[test]
    fn parse_http2_status() {
        let raw = "HTTP/2 404 Not Found\r\nContent-Type: text/html\r\n\r\n";
        let (status, headers) = parse_http_headers(raw);
        assert_eq!(status, Some(404));
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn parse_headers_lowercase_keys() {
        let raw =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Custom-Header: value\r\n\r\n";
        let (_, headers) = parse_http_headers(raw);
        // Keys should be lowercased
        assert!(headers.iter().any(|(k, _)| k == "content-type"));
        assert!(headers.iter().any(|(k, _)| k == "x-custom-header"));
    }

    #[test]
    fn parse_headers_preserves_colons_in_values() {
        let raw = "HTTP/1.1 200 OK\r\nLocation: https://example.com/a:b\r\n\r\n";
        let (_, headers) = parse_http_headers(raw);
        assert_eq!(
            headers.iter().find(|(k, _)| k == "location").unwrap().1,
            "https://example.com/a:b"
        );
    }

    #[test]
    fn parse_http_headers_skips_lines_without_colon() {
        let raw = "HTTP/1.1 200 OK\r\nnot-a-header\r\nContent-Type: text/plain\r\n\r\n";
        let (_, headers) = parse_http_headers(raw);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "content-type");
    }

    #[test]
    fn parse_wget_empty() {
        let (status, headers) = parse_wget_headers("");
        assert_eq!(status, None);
        assert!(headers.is_empty());
    }

    #[test]
    fn parse_wget_redirect_chain() {
        let stderr = r#"
  HTTP/1.1 301 Moved Permanently
  Location: https://new.example.com
  HTTP/1.1 200 OK
  Content-Type: text/html
"#;
        let (status, headers) = parse_wget_headers(stderr);
        assert_eq!(status, Some(200));
        assert!(headers.iter().any(|(k, _)| k == "content-type"));
    }

    #[test]
    fn parse_wget_skips_lines_with_spaces_in_key() {
        let stderr = r#"
  HTTP/1.1 200 OK
  Content-Type: text/html
  not a header line
"#;
        let (status, headers) = parse_wget_headers(stderr);
        assert_eq!(status, Some(200));
        // "not a header line" has spaces in key, should be skipped
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn parse_wget_returns_none_when_no_http_status_seen() {
        let stderr = "Resolving example.com... connected.";
        let (status, headers) = parse_wget_headers(stderr);
        assert_eq!(status, None);
        assert!(headers.is_empty());
    }

    #[test]
    fn classify_402_empty_headers() {
        let outcome = classify_402(&[], None, "https://example.com");
        assert!(matches!(outcome, RunOutcome::UnknownPaymentRequired { .. }));
    }

    #[test]
    fn classify_402_preserves_resource_url() {
        let outcome = classify_402(&[], None, "https://api.example.com/data");
        match outcome {
            RunOutcome::UnknownPaymentRequired { resource_url, .. } => {
                assert_eq!(resource_url, "https://api.example.com/data");
            }
            _ => panic!("Expected UnknownPaymentRequired"),
        }
    }

    #[test]
    fn check_command_exists_finds_ls() {
        // `ls` should exist on any unix system
        assert!(check_command_exists("ls").is_ok());
    }

    #[test]
    fn check_command_exists_fails_for_nonexistent() {
        let result = check_command_exists("nonexistent_command_xyz_12345");
        assert!(result.is_err());
    }
}
