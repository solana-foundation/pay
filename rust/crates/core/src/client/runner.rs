use std::process::{Command, Stdio};

use tempfile::NamedTempFile;
use tracing::{debug, info};

use crate::client::mpp;
use crate::client::x402;
use crate::{Error, Result};

/// The outcome of running a wrapped command.
#[derive(Debug)]
pub enum RunOutcome {
    /// The server returned 402 with an MPP charge challenge.
    MppChallenge {
        challenge: Box<mpp::Challenge>,
        alternatives: Vec<mpp::Challenge>,
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
        challenge: Box<x402::Challenge>,
        resource_url: String,
    },
    /// The server returned 402 with an auth-only x402 SIWX challenge.
    X402SignInChallenge {
        challenge: Box<x402::SiwxAuthChallenge>,
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
    if is_passthrough_metadata_request(user_args) {
        return run_plain_command("curl", user_args);
    }

    validate_curl_args_against_catalog(user_args)?;
    run_curl_inner(user_args, &[])
}

/// Run `curl` with extra headers injected (used for retry after payment).
pub fn run_curl_with_headers(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    run_curl_inner(user_args, extra_headers)
}

/// Validate a curl invocation against cached Pay catalog OpenAPI metadata.
pub fn validate_curl_args_against_catalog(user_args: &[String]) -> Result<()> {
    let request = ParsedCurlRequest::from_args(user_args);
    if let Some(url) = request.url.as_deref() {
        crate::skills::validate_cached_catalog_request(
            &request.method,
            url,
            request.body.as_deref(),
        )?;
    }
    Ok(())
}

fn run_curl_inner(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    check_command_exists("curl")?;

    let header_file = NamedTempFile::new()?;
    let header_path = header_file.path();
    let body_file = NamedTempFile::new()?;
    let body_path = body_file.path();

    debug!(args = ?user_args, extra = ?extra_headers, "Running curl");

    // Body goes to `-o body_file` so we can swallow it on 402; stdout is piped
    // so curl's `-w` writeout (which it emits to stdout after the transfer) is
    // captured and re-emitted on the success path. Without this, `pay curl -w
    // '%{http_code}'` silently drops the writeout because we'd discard stdout.
    let mut cmd = Command::new("curl");
    cmd.args(user_args);
    for h in extra_headers {
        cmd.arg("-H").arg(h);
    }
    cmd.arg("-D").arg(header_path);
    cmd.arg("-o").arg(body_path);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output()?;
    let exit_code = output.status.code().unwrap_or(1);
    let headers_raw = std::fs::read_to_string(header_path).unwrap_or_default();
    let body = std::fs::read_to_string(body_path).unwrap_or_default();
    let (status_code, headers) = parse_http_headers(&headers_raw);
    let url = find_url_in_args(user_args).unwrap_or_default();

    debug!(?status_code, exit_code, "curl finished");

    if status_code == Some(402) {
        // Swallow stderr/stdout/body on 402 — CLI handles display
        return Ok(classify_402(&headers, Some(&body), &url));
    }

    // Not 402 — re-emit stderr (progress bar etc.), body, then any -w writeout
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    print!("{body}");
    let writeout = String::from_utf8_lossy(&output.stdout);
    if !writeout.is_empty() {
        print!("{writeout}");
    }
    Ok(RunOutcome::Completed {
        exit_code,
        body: None,
    })
}

/// Run `wget` with the given user args, detecting 402 + MPP challenges.
pub fn run_wget(user_args: &[String]) -> Result<RunOutcome> {
    if is_passthrough_metadata_request(user_args) {
        return run_plain_command("wget", user_args);
    }

    validate_wget_args_against_catalog(user_args)?;
    run_wget_inner(user_args, &[])
}

/// Run `http` (HTTPie) with the given user args, detecting 402 + MPP challenges.
///
/// HTTPie uses positional `Header:Value` request items rather than `-H` flags,
/// so `extra_headers` for retry are appended as positional args.
pub fn run_httpie(user_args: &[String]) -> Result<RunOutcome> {
    if is_passthrough_metadata_request(user_args) {
        return run_plain_command("http", user_args);
    }

    run_httpie_inner(user_args, &[])
}

/// Run `http` with extra headers injected (used for retry after payment).
///
/// Each entry in `extra_headers` is the literal HTTPie request item
/// (e.g. `"Authorization:Bearer …"`), already formatted by the caller.
pub fn run_httpie_with_headers(
    user_args: &[String],
    extra_headers: &[String],
) -> Result<RunOutcome> {
    run_httpie_inner(user_args, extra_headers)
}

/// Run `wget` with extra headers injected (used for retry after payment).
pub fn run_wget_with_headers(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    run_wget_inner(user_args, extra_headers)
}

/// Validate a wget invocation against cached Pay catalog OpenAPI metadata.
pub fn validate_wget_args_against_catalog(user_args: &[String]) -> Result<()> {
    let request = ParsedWgetRequest::from_args(user_args);
    if let Some(url) = request.url.as_deref() {
        crate::skills::validate_cached_catalog_request(
            &request.method,
            url,
            request.body.as_deref(),
        )?;
    }
    Ok(())
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

fn run_httpie_inner(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    use std::io::IsTerminal;

    check_command_exists("http")?;

    debug!(args = ?user_args, extra = ?extra_headers, "Running httpie");

    // HTTPie has no `-D <file>` equivalent, so we capture stdout and parse the
    // response status from the first `HTTP/x.y <code>` line. We force two flags
    // *after* the user's args so they always win:
    //   - `--print=hb` — httpie's default when piped is body-only, which would
    //     hide the status line our parser needs.
    //   - `--pretty=all` — only when our parent stdout is a TTY, so the user
    //     sees colors despite our pipe (httpie would otherwise auto-disable
    //     them). We strip ANSI codes for parsing.
    let stdout_is_tty = std::io::stdout().is_terminal();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let mut cmd = Command::new("http");
    cmd.args(user_args);
    for h in extra_headers {
        cmd.arg(h);
    }
    cmd.arg("--print=hb");
    if stdout_is_tty {
        cmd.arg("--pretty=all");
    }
    // When parent stdin isn't a real TTY (e.g. CI / agent shell), httpie reads
    // it as request body and conflicts with `field=value` items. Tell it to
    // ignore stdin in that case; if a user is genuinely piping data in,
    // they're expected to pass `--ignore-stdin` themselves or use `@file`.
    if !stdin_is_tty {
        cmd.arg("--ignore-stdin");
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output()?;
    let exit_code = output.status.code().unwrap_or(1);
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    let stderr_text = String::from_utf8_lossy(&output.stderr);
    let url = find_url_in_args(user_args).unwrap_or_default();

    let (status_code, headers, body) = parse_httpie_output(&stdout_text);

    debug!(?status_code, exit_code, "httpie finished");

    if status_code == Some(402) {
        // Swallow stdout/stderr on 402 — CLI handles display
        return Ok(classify_402(&headers, body.as_deref(), &url));
    }

    if !stderr_text.is_empty() {
        eprint!("{stderr_text}");
    }
    print!("{stdout_text}");
    Ok(RunOutcome::Completed {
        exit_code,
        body: None,
    })
}

/// Parse HTTPie's combined stdout into `(status, response_headers, body)`.
///
/// HTTPie writes the response as:
/// ```text
/// HTTP/1.1 <code> <reason>
/// Header: value
/// …
/// <blank line>
/// <body>
/// ```
/// In verbose mode (`-v`) the request is printed first, so we take the LAST
/// `HTTP/x.y` line as the response status and the headers that follow it.
/// ANSI escapes (from `--pretty=all`) are stripped before parsing.
pub(crate) fn parse_httpie_output(
    raw: &str,
) -> (Option<u16>, Vec<(String, String)>, Option<String>) {
    let cleaned = strip_ansi(raw);
    let lines: Vec<&str> = cleaned.lines().collect();

    let mut status_code = None;
    let mut headers = Vec::new();
    let mut body_lines: Option<Vec<&str>> = None;
    let mut in_response_headers = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed_full = line.trim();

        if trimmed_full.starts_with("HTTP/") {
            // Response status line. Reset — the LAST HTTP/ line wins so a
            // verbose request line earlier in the stream doesn't confuse us.
            status_code = trimmed_full
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok());
            headers.clear();
            body_lines = None;
            in_response_headers = true;
            continue;
        }

        if in_response_headers {
            if trimmed_full.is_empty() {
                body_lines = Some(lines[(i + 1)..].to_vec());
                break;
            }
            if let Some((k, v)) = trimmed_full.split_once(':') {
                let key = k.trim();
                if !key.is_empty() && !key.contains(' ') {
                    headers.push((key.to_lowercase(), v.trim().to_string()));
                }
            }
        }
    }

    let body = body_lines.map(|lines| lines.join("\n"));
    (status_code, headers, body)
}

/// Strip ANSI CSI escape sequences (`\x1b[...m` and friends) from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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

    // Parse both protocols — multi-chain endpoints may advertise both
    // x402 (Solana + Base) and Tempo/MPP (EVM-only).
    //
    // Some servers use `payment-required` instead of `x-payment-required`
    // for x402. If the standard parse fails, try decoding `payment-required`
    // as base64 JSON and re-parse.
    let x402_challenge = x402::parse(headers, body).or_else(|| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(solana_x402::PAYMENT_REQUIRED_HEADER))
            .and_then(|(_, v)| {
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD.decode(v).ok()?;
                let json_str = String::from_utf8(decoded).ok()?;
                // Re-parse with the decoded JSON as the body
                x402::parse(&[], Some(&json_str))
            })
    });
    let mpp_challenges = mpp::parse_headers(headers);
    let x402_siwx_challenge = x402::parse_siwx_auth(headers, body);

    // x402::parse (from solana_x402) only returns Some when a Solana-
    // compatible `accepts` entry exists — it's already a Solana filter.
    // MPP is chain-agnostic at the parse level, so we need to validate
    // the recipient is a valid Solana pubkey.
    // Session MPP: the method field ("solana") indicates chain support.
    // Session requests don't use ChargeRequest so mpp_is_solana doesn't apply.
    if let Some(challenge) = mpp_challenges
        .iter()
        .find(|challenge| challenge.intent.as_str() == "session")
    {
        let is_solana_method = challenge.method.as_str() == "solana";
        if is_solana_method {
            info!(
                resource = resource_url,
                "Detected MPP session challenge (Solana)"
            );
            return RunOutcome::SessionChallenge {
                challenge: Box::new(challenge.clone()),
                resource_url: resource_url.to_string(),
            };
        }
        // Non-Solana session — fall through to x402 or error.
    }

    // Prefer MPP for one-shot Solana payments (native protocol).
    let mut charge_challenges: Vec<mpp::Challenge> = mpp_challenges
        .iter()
        .filter(|challenge| solana_mpp::client::is_solana_charge_challenge(challenge))
        .cloned()
        .collect();
    if !charge_challenges.is_empty() {
        let challenge = charge_challenges.remove(0);
        info!(resource = resource_url, "Detected MPP challenge (Solana)");
        return RunOutcome::MppChallenge {
            challenge: Box::new(challenge),
            alternatives: charge_challenges,
            resource_url: resource_url.to_string(),
        };
    }

    // Fall back to x402 if it has a Solana path.
    if let Some(challenge) = x402_challenge {
        info!(resource = resource_url, "Detected x402 challenge (Solana)");
        return RunOutcome::X402Challenge {
            challenge: Box::new(challenge),
            resource_url: resource_url.to_string(),
        };
    }

    if let Some(challenge) = x402_siwx_challenge {
        info!(resource = resource_url, "Detected x402 sign-in challenge");
        return RunOutcome::X402SignInChallenge {
            challenge: Box::new(challenge),
            resource_url: resource_url.to_string(),
        };
    }

    // Neither protocol supports Solana — tell the user clearly.
    if !mpp_challenges.is_empty() {
        return RunOutcome::PaymentRejected {
            reason: "Server requires payment but only accepts non-Solana chains \
                     (e.g. Base/EVM). This endpoint is not compatible with `pay`. \
                     Check if the provider supports Solana USDC."
                .to_string(),
            retryable: false,
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

fn is_passthrough_metadata_request(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-h" | "--help" | "--manual" | "-V" | "--version"
        ) || arg.starts_with("--help=")
    })
}

fn run_plain_command(program: &str, args: &[String]) -> Result<RunOutcome> {
    check_command_exists(program)?;

    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    Ok(RunOutcome::Completed {
        exit_code: status.code().unwrap_or(1),
        body: None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedCurlRequest {
    url: Option<String>,
    method: String,
    body: Option<String>,
}

impl ParsedCurlRequest {
    fn from_args(args: &[String]) -> Self {
        let mut url = None;
        let mut explicit_method = None;
        let mut body = None;
        let mut force_get = false;
        let mut i = 0;

        while i < args.len() {
            let arg = &args[i];
            match arg.as_str() {
                "-X" | "--request" => {
                    if let Some(value) = args.get(i + 1) {
                        explicit_method = Some(value.to_ascii_uppercase());
                        i += 1;
                    }
                }
                "--url" => {
                    if let Some(value) = args.get(i + 1) {
                        url = Some(value.clone());
                        i += 1;
                    }
                }
                "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-ascii"
                | "--data-urlencode" | "--json" => {
                    if let Some(value) = args.get(i + 1) {
                        append_curl_body(&mut body, value);
                        i += 1;
                    }
                }
                "-G" | "--get" => {
                    force_get = true;
                }
                "-I" | "--head" => {
                    explicit_method = Some("HEAD".to_string());
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--request=") {
                        explicit_method = Some(value.to_ascii_uppercase());
                    } else if let Some(value) = arg.strip_prefix("--url=") {
                        url = Some(value.to_string());
                    } else if let Some(value) = arg.strip_prefix("--data=") {
                        append_curl_body(&mut body, value);
                    } else if let Some(value) = arg.strip_prefix("--data-raw=") {
                        append_curl_body(&mut body, value);
                    } else if let Some(value) = arg.strip_prefix("--data-binary=") {
                        append_curl_body(&mut body, value);
                    } else if let Some(value) = arg.strip_prefix("--data-ascii=") {
                        append_curl_body(&mut body, value);
                    } else if let Some(value) = arg.strip_prefix("--data-urlencode=") {
                        append_curl_body(&mut body, value);
                    } else if let Some(value) = arg.strip_prefix("--json=") {
                        append_curl_body(&mut body, value);
                    } else if arg.starts_with("-X") && arg.len() > 2 {
                        explicit_method = Some(arg[2..].to_ascii_uppercase());
                    } else if arg.starts_with("-d") && arg.len() > 2 {
                        append_curl_body(&mut body, &arg[2..]);
                    } else if url.is_none()
                        && (arg.starts_with("http://") || arg.starts_with("https://"))
                    {
                        url = Some(arg.clone());
                    }
                }
            }
            i += 1;
        }

        let method = explicit_method.unwrap_or_else(|| {
            if force_get || body.is_none() {
                "GET".to_string()
            } else {
                "POST".to_string()
            }
        });

        Self { url, method, body }
    }
}

fn append_curl_body(body: &mut Option<String>, value: &str) {
    match body {
        Some(body) if !body.is_empty() => {
            body.push('&');
            body.push_str(value);
        }
        Some(body) => body.push_str(value),
        None => *body = Some(value.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedWgetRequest {
    url: Option<String>,
    method: String,
    body: Option<String>,
}

impl ParsedWgetRequest {
    fn from_args(args: &[String]) -> Self {
        let mut url = None;
        let mut explicit_method = None;
        let mut body = None;
        let mut post_body_seen = false;
        let mut i = 0;

        while i < args.len() {
            let arg = &args[i];
            match arg.as_str() {
                "--method" => {
                    if let Some(value) = args.get(i + 1) {
                        explicit_method = Some(value.to_ascii_uppercase());
                        i += 1;
                    }
                }
                "--post-data" | "--body-data" => {
                    if let Some(value) = args.get(i + 1) {
                        body = Some(value.clone());
                        post_body_seen = true;
                        i += 1;
                    }
                }
                "--spider" => {
                    explicit_method.get_or_insert_with(|| "HEAD".to_string());
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--method=") {
                        explicit_method = Some(value.to_ascii_uppercase());
                    } else if let Some(value) = arg.strip_prefix("--post-data=") {
                        body = Some(value.to_string());
                        post_body_seen = true;
                    } else if let Some(value) = arg.strip_prefix("--body-data=") {
                        body = Some(value.to_string());
                        post_body_seen = true;
                    } else if matches!(
                        arg.as_str(),
                        "--post-file" | "--body-file" | "--post-file=" | "--body-file="
                    ) {
                        post_body_seen = true;
                        if !arg.ends_with('=') && args.get(i + 1).is_some() {
                            i += 1;
                        }
                    } else if arg.starts_with("--post-file=") || arg.starts_with("--body-file=") {
                        post_body_seen = true;
                    } else if url.is_none()
                        && (arg.starts_with("http://") || arg.starts_with("https://"))
                    {
                        url = Some(arg.clone());
                    }
                }
            }
            i += 1;
        }

        let method = explicit_method.unwrap_or_else(|| {
            if post_body_seen {
                "POST".to_string()
            } else {
                "GET".to_string()
            }
        });

        Self { url, method, body }
    }
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
    fn passthrough_metadata_request_detects_help_and_version() {
        assert!(is_passthrough_metadata_request(&["--help".to_string()]));
        assert!(is_passthrough_metadata_request(&["-h".to_string()]));
        assert!(is_passthrough_metadata_request(&["--help=all".to_string()]));
        assert!(is_passthrough_metadata_request(&["--version".to_string()]));
        assert!(is_passthrough_metadata_request(&["-V".to_string()]));
    }

    #[test]
    fn passthrough_metadata_request_ignores_normal_requests() {
        let args = vec![
            "-H".to_string(),
            "X-Mode: help".to_string(),
            "https://example.com".to_string(),
        ];
        assert!(!is_passthrough_metadata_request(&args));
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
    fn classify_402_preserves_multiple_mpp_charge_challenges() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let header_for = |currency: &str| {
            let request_json = serde_json::json!({
                "amount": "1000000",
                "currency": currency,
                "recipient": "So11111111111111111111111111111111111111112",
                "methodDetails": { "network": "devnet" }
            });
            let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request_json).unwrap());
            (
                "www-authenticate".to_string(),
                format!(
                    "Payment id=\"{currency}\", realm=\"test\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
                ),
            )
        };
        let headers = vec![header_for("USDC"), header_for("USDT"), header_for("CASH")];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        match outcome {
            RunOutcome::MppChallenge {
                challenge,
                alternatives,
                ..
            } => {
                let first: solana_mpp::ChargeRequest = challenge.request.decode().unwrap();
                assert_eq!(first.currency, "USDC");
                assert_eq!(alternatives.len(), 2);
            }
            other => panic!("expected MppChallenge, got {other:?}"),
        }
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
        let headers = vec![(
            solana_x402::X402_V1_PAYMENT_REQUIRED_HEADER.to_string(),
            requirements.to_string(),
        )];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(matches!(outcome, RunOutcome::X402Challenge { .. }));
    }

    #[test]
    fn classify_402_with_x402_siwx_auth_only_header() {
        use base64::Engine;

        let payment_required = serde_json::json!({
            solana_x402::X402_VERSION_FIELD: solana_x402::X402_VERSION_V2,
            "resource": {
                "url": "https://example.com/resource",
                "description": "API access"
            },
            "accepts": [],
            "extensions": {
                "sign-in-with-x": {
                    "domain": "example.com",
                    "uri": "https://example.com",
                    "version": "1",
                    "nonce": "nonce-123",
                    "issuedAt": "2026-04-27T00:00:00Z",
                    "supportedChains": [{
                        "chainId": solana_x402::exact::SOLANA_MAINNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        });
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(payment_required.to_string().as_bytes());
        let headers = vec![(solana_x402::PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        let outcome = classify_402(&headers, None, "https://example.com/resource");

        match outcome {
            RunOutcome::X402SignInChallenge {
                challenge,
                resource_url,
            } => {
                assert_eq!(challenge.extension.nonce, "nonce-123");
                assert_eq!(resource_url, "https://example.com/resource");
            }
            other => panic!("expected X402SignInChallenge, got {other:?}"),
        }
    }

    #[test]
    fn classify_402_prefers_payment_when_siwx_extends_payment_challenge() {
        use base64::Engine;

        let selected = serde_json::json!({
            "scheme": solana_x402::exact::EXACT_SCHEME,
            "network": solana_x402::exact::SOLANA_MAINNET,
            "amount": "10000",
            "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "payTo": "6cvgmdrsVxyiuPzqMCSBnS7fAmA5Mk2VG4BcfVhC8jdC",
            "maxTimeoutSeconds": 300
        });
        let payment_required = serde_json::json!({
            solana_x402::X402_VERSION_FIELD: solana_x402::X402_VERSION_V2,
            "accepts": [selected],
            "extensions": {
                "sign-in-with-x": {
                    "domain": "example.com",
                    "uri": "https://example.com",
                    "version": "1",
                    "nonce": "nonce-123",
                    "issuedAt": "2026-04-27T00:00:00Z",
                    "supportedChains": [{
                        "chainId": solana_x402::exact::SOLANA_MAINNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        });
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(payment_required.to_string().as_bytes());
        let headers = vec![(solana_x402::PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        let outcome = classify_402(&headers, None, "https://example.com/resource");

        match outcome {
            RunOutcome::X402Challenge { challenge, .. } => {
                assert_eq!(challenge.requirements.amount, "10000");
                assert!(challenge.siwx.is_some());
            }
            other => panic!("expected X402Challenge, got {other:?}"),
        }
    }

    #[test]
    fn classify_402_rejects_evm_only_mpp_with_clear_error() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        // MPP challenge with EVM-style Tempo recipient (not Solana)
        let request_json = serde_json::json!({
            "amount": "10000",
            "currency": "0x20c00000000000000000000b9537d11c60e8b50",
            "methodDetails": { "chainId": 4217 },
            "recipient": "0x325bdF6F7efAB24a2210c48c1b64cAb2eAe1d430"
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request_json).unwrap());
        let headers = vec![(
            "www-authenticate".to_string(),
            format!(
                "Payment id=\"test\", realm=\"test\", method=\"tempo\", intent=\"charge\", request=\"{b64}\""
            ),
        )];

        // EVM-only MPP with no x402 fallback → clear rejection
        let outcome = classify_402(&headers, None, "https://evm-only.example.com/api");
        match outcome {
            RunOutcome::PaymentRejected { reason, .. } => {
                assert!(
                    reason.contains("non-Solana"),
                    "Expected non-Solana message, got: {reason}"
                );
            }
            other => panic!("Expected PaymentRejected, got: {other:?}"),
        }
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
    fn parsed_curl_request_extracts_method_url_and_json_body() {
        let args = vec![
            "--json".to_string(),
            r#"{"query":"solana"}"#.to_string(),
            "https://example.com/api/search".to_string(),
        ];

        assert_eq!(
            ParsedCurlRequest::from_args(&args),
            ParsedCurlRequest {
                url: Some("https://example.com/api/search".to_string()),
                method: "POST".to_string(),
                body: Some(r#"{"query":"solana"}"#.to_string()),
            }
        );
    }

    #[test]
    fn parsed_curl_request_honors_explicit_request_and_url_flags() {
        let args = vec![
            "--request=PATCH".to_string(),
            "--data-raw".to_string(),
            r#"{"name":"pay"}"#.to_string(),
            "--url".to_string(),
            "https://example.com/api/item".to_string(),
        ];

        assert_eq!(
            ParsedCurlRequest::from_args(&args),
            ParsedCurlRequest {
                url: Some("https://example.com/api/item".to_string()),
                method: "PATCH".to_string(),
                body: Some(r#"{"name":"pay"}"#.to_string()),
            }
        );
    }

    #[test]
    fn parsed_wget_request_extracts_post_data() {
        let args = vec![
            "--post-data".to_string(),
            r#"{"productUrl":"https://example.com/item"}"#.to_string(),
            "https://api.example.com/x402/buy".to_string(),
        ];

        assert_eq!(
            ParsedWgetRequest::from_args(&args),
            ParsedWgetRequest {
                url: Some("https://api.example.com/x402/buy".to_string()),
                method: "POST".to_string(),
                body: Some(r#"{"productUrl":"https://example.com/item"}"#.to_string()),
            }
        );
    }

    #[test]
    fn parsed_wget_request_honors_explicit_method() {
        let args = vec![
            "--method=PUT".to_string(),
            "--body-data={\"name\":\"pay\"}".to_string(),
            "https://api.example.com/items/1".to_string(),
        ];

        assert_eq!(
            ParsedWgetRequest::from_args(&args),
            ParsedWgetRequest {
                url: Some("https://api.example.com/items/1".to_string()),
                method: "PUT".to_string(),
                body: Some(r#"{"name":"pay"}"#.to_string()),
            }
        );
    }

    #[test]
    fn parsed_wget_request_body_file_defaults_to_post_without_body_for_validation() {
        let args = vec![
            "--body-file".to_string(),
            "payload.json".to_string(),
            "https://api.example.com/items".to_string(),
        ];

        assert_eq!(
            ParsedWgetRequest::from_args(&args),
            ParsedWgetRequest {
                url: Some("https://api.example.com/items".to_string()),
                method: "POST".to_string(),
                body: None,
            }
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

    // ── parse_httpie_output / strip_ansi ─────────────────────────────────

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        let raw = "\x1b[34mHTTP\x1b[39;49;00m/\x1b[34m1.1\x1b[39;49;00m \x1b[34m200\x1b[39;49;00m \x1b[36mOK\x1b[39;49;00m";
        assert_eq!(strip_ansi(raw), "HTTP/1.1 200 OK");
    }

    #[test]
    fn strip_ansi_passes_through_plain_text() {
        assert_eq!(
            strip_ansi("plain text\nno escapes"),
            "plain text\nno escapes"
        );
    }

    #[test]
    fn parse_httpie_basic_response() {
        let raw = "HTTP/1.1 200 OK\nContent-Type: application/json\nContent-Length: 13\n\n{\"ok\":true}\n";
        let (status, headers, body) = parse_httpie_output(raw);
        assert_eq!(status, Some(200));
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0, "content-type");
        // `lines()` strips line terminators; the rejoined body has no trailing \n.
        assert_eq!(body.as_deref(), Some("{\"ok\":true}"));
    }

    #[test]
    fn parse_httpie_402_response() {
        let raw = "HTTP/1.1 402 Payment Required\nWWW-Authenticate: Payment realm=\"x\"\n\n{\"error\":\"verification_failed\",\"message\":\"bad\",\"retryable\":false}";
        let (status, headers, body) = parse_httpie_output(raw);
        assert_eq!(status, Some(402));
        assert!(headers.iter().any(|(k, _)| k == "www-authenticate"));
        assert!(body.as_deref().unwrap().contains("verification_failed"));
    }

    #[test]
    fn parse_httpie_verbose_mode_picks_response_status() {
        // -v prints request first (with `METHOD /path HTTP/1.1` line, NOT
        // starting with `HTTP/`), then response.
        let raw = "GET /api HTTP/1.1\nHost: example.com\nUser-Agent: HTTPie/3.2.4\n\nHTTP/1.1 200 OK\nContent-Type: application/json\n\n{\"ok\":1}";
        let (status, headers, body) = parse_httpie_output(raw);
        assert_eq!(status, Some(200));
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "content-type" && v == "application/json")
        );
        // Request headers (host, user-agent) must not bleed into response
        // headers — `Host` is set by the client, never echoed by the server here.
        assert!(!headers.iter().any(|(k, _)| k == "host"));
        assert_eq!(body.as_deref(), Some("{\"ok\":1}"));
    }

    #[test]
    fn parse_httpie_http2_status() {
        let raw = "HTTP/2 404 Not Found\nContent-Type: text/html\n\n<html/>";
        let (status, _, _) = parse_httpie_output(raw);
        assert_eq!(status, Some(404));
    }

    #[test]
    fn parse_httpie_handles_pretty_ansi() {
        // Mimics --pretty=all output: status + first header colorized.
        let raw = "\x1b[34mHTTP\x1b[39;49;00m/\x1b[34m1.1\x1b[39;49;00m \x1b[34m402\x1b[39;49;00m \x1b[36mPayment Required\x1b[39;49;00m\n\x1b[36mContent-Type\x1b[39;49;00m: application/json\n\n{\"error\":\"x\"}";
        let (status, headers, body) = parse_httpie_output(raw);
        assert_eq!(status, Some(402));
        assert!(headers.iter().any(|(k, _)| k == "content-type"));
        assert_eq!(body.as_deref(), Some("{\"error\":\"x\"}"));
    }

    #[test]
    fn parse_httpie_no_body() {
        // HEAD response or 204: headers but no blank line + body.
        let raw = "HTTP/1.1 204 No Content\nDate: now\n";
        let (status, headers, body) = parse_httpie_output(raw);
        assert_eq!(status, Some(204));
        assert_eq!(headers.len(), 1);
        assert!(body.is_none());
    }

    #[test]
    fn parse_httpie_empty_input() {
        let (status, headers, body) = parse_httpie_output("");
        assert_eq!(status, None);
        assert!(headers.is_empty());
        assert!(body.is_none());
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
