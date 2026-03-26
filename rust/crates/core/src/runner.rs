use std::process::{Command, Stdio};

use tempfile::NamedTempFile;
use tracing::{debug, info};

use solana_x402::protocol::methods::solana::PaymentRequirements;

use crate::mpp;
use crate::x402;
use crate::{Error, Result};

/// The outcome of running a wrapped command.
#[derive(Debug)]
pub enum RunOutcome {
    /// The server returned 402 with an MPP challenge.
    MppChallenge {
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
        // Swallow stderr on 402
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
    // Check for MPP challenge in www-authenticate header
    if let Some(www_auth) = headers.iter().find(|(k, _)| k == "www-authenticate")
        && let Some(challenge) = mpp::parse(&www_auth.1)
    {
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

// ── httpie ──

/// Run `http` (httpie) with the given user args, detecting 402 + MPP challenges.
///
/// Adds `--print=hb` to capture headers+body on stdout, parses headers from output.
pub fn run_httpie(user_args: &[String]) -> Result<RunOutcome> {
    run_httpie_inner(user_args, &[])
}

/// Run `http` (httpie) with extra headers injected (used for retry after payment).
pub fn run_httpie_with_headers(
    user_args: &[String],
    extra_headers: &[String],
) -> Result<RunOutcome> {
    run_httpie_inner(user_args, extra_headers)
}

fn run_httpie_inner(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    // httpie can be invoked as `http` or `https`
    check_command_exists("http")?;

    debug!(args = ?user_args, extra = ?extra_headers, "Running httpie");

    // First pass: capture headers + body to parse status code
    let mut cmd = Command::new("http");
    cmd.arg("--print=hb");
    cmd.args(user_args);
    for h in extra_headers {
        cmd.arg(h.as_str());
    }
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = cmd.output()?;
    let exit_code = output.status.code().unwrap_or(1);
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    let stderr_text = String::from_utf8_lossy(&output.stderr);

    let (status_code, headers, _body) = parse_httpie_output(&stdout_text);
    let url = find_url_in_args(user_args).unwrap_or_default();

    debug!(?status_code, exit_code, "httpie finished");

    if status_code == Some(402) {
        return Ok(classify_402(&headers, None, &url));
    }

    // Not 402 — re-emit everything
    if !stderr_text.is_empty() {
        eprint!("{stderr_text}");
    }
    print!("{stdout_text}");

    Ok(RunOutcome::Completed {
        exit_code,
        body: None,
    })
}

/// Parse httpie `--print=hb` output into status code, headers, and body.
fn parse_httpie_output(output: &str) -> (Option<u16>, Vec<(String, String)>, &str) {
    // Split on first double newline: headers block, then body
    let (header_block, body) = output.split_once("\n\n").unwrap_or((output, ""));

    let (status_code, headers) = parse_http_headers(header_block);
    (status_code, headers, body)
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
    fn classify_402_without_mpp() {
        let headers = vec![("content-type".to_string(), "text/html".to_string())];
        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(matches!(outcome, RunOutcome::UnknownPaymentRequired { .. }));
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
}
