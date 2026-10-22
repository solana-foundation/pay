use std::process::{Command, Stdio};

use tempfile::NamedTempFile;
use tracing::{debug, info};

use crate::client::mpp;
use crate::client::subscription;
use crate::client::x402;
use crate::{ClientApp, Error, Result};

/// Payment challenges advertised by the first 402 response, decoded into
/// JSON and grouped by wire protocol for verbose CLI diagnostics.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DecodedPaymentChallenges {
    /// Every x402 offer from the decoded `accepts` list (or the legacy
    /// single-requirement payload), in server order.
    pub x402: Vec<serde_json::Value>,
    /// Every MPP challenge with its base64url payload fields decoded as JSON.
    pub mpp: Vec<serde_json::Value>,
}

impl DecodedPaymentChallenges {
    pub fn is_empty(&self) -> bool {
        self.x402.is_empty() && self.mpp.is_empty()
    }
}

/// The outcome of running a wrapped command.
#[derive(Debug)]
pub enum RunOutcome {
    /// The server returned 402 with an MPP charge challenge.
    ///
    /// `x402_alternative` carries an x402 charge option advertised on the same
    /// 402, when present. MPP is preferred by default, but a balance-aware
    /// selector may settle the x402 option instead when the wallet can't fund
    /// any MPP challenge (e.g. MPP wants USDG, wallet holds only USDC offered
    /// via x402). See [`mpp::choose_payment`](crate::client::mpp::choose_payment).
    MppChallenge {
        challenge: Box<mpp::Challenge>,
        alternatives: Vec<mpp::Challenge>,
        x402_alternative: Option<Box<x402::Challenge>>,
        /// Every x402 `upto` currency advertised on the same 402, carried so the
        /// balance- and cost-aware selector can settle whichever is cheapest (or
        /// the only fundable one) — not just the first. See [`mpp::choose_payment`].
        x402_upto_accepts: Vec<pay_kit::x402::upto::UptoRequirements>,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The server returned 402 with an MPP session challenge (intent="session").
    /// Session payments require a stateful client with an MPP payment channel.
    ///
    /// `fallback` is what the same 402 resolves to with the session offer set
    /// aside (a charge or x402 offer), when it advertised one. A signer that
    /// cannot open this session takes it instead; see
    /// [`RunOutcome::for_signer`].
    SessionChallenge {
        challenge: Box<mpp::Challenge>,
        fallback: Option<Box<RunOutcome>>,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The server returned 402 with an MPP subscription challenge
    /// (intent="subscription"). The activation transaction creates the
    /// recurring delegation and collects the first-period charge in one
    /// atomic transaction; renewals are server-driven and don't pass back
    /// through this enum.
    ///
    /// `authenticate` carries the sibling `intent="authenticate"`
    /// challenge from the same 402 response when the server emitted one
    /// (pay-server emits both). The caller signs it in the same Touch ID
    /// session as the activation tx and caches the resulting token in
    /// `accounts.yml` so subsequent requests within the billing period
    /// skip the 402 dance entirely.
    SubscriptionChallenge {
        challenge: Box<mpp::Challenge>,
        authenticate: Option<Box<mpp::Challenge>>,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The server returned 402 with an x402 challenge.
    X402Challenge {
        challenge: Box<x402::Challenge>,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The server returned 402 with an x402 `upto` (usage-metered) challenge —
    /// authorize a ceiling via a payment channel; the operator settles the
    /// actual amount after serving.
    X402UptoChallenge {
        challenge: Box<x402::UptoChallenge>,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The server returned 402 with an x402 `batch-settlement` challenge —
    /// one escrow channel backs many cheap requests, each authorized by a
    /// cumulative voucher rather than its own transaction.
    ///
    /// Like an MPP session, and unlike `upto`, this is stateful: the caller
    /// keeps the channel in a [`crate::client::batch::BatchChannelCache`] and
    /// reuses it across requests. Callers can choose a larger deposit to
    /// amortize funding transactions across many calls.
    X402BatchChallenge {
        challenge: Box<x402::BatchChallenge>,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The server returned 402 with an x402 `sign-in-with-x` challenge.
    ///
    /// When the same 402 also offered a payment option, `payment_fallback`
    /// carries it: the client prefers signing in (to spend existing credits)
    /// but can fall back to paying if sign-in doesn't grant access (e.g. the
    /// wallet hasn't paid / has no credits yet).
    X402SignInChallenge {
        challenge: Box<x402::SiwxAuthChallenge>,
        payment_fallback: Option<Box<x402::Challenge>>,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The server returned 402 but without a recognized payment protocol.
    UnknownPaymentRequired {
        headers: Vec<(String, String)>,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The server returned 402 with a `verification_failed` body — this is a
    /// retry response telling the client *why* the previously-submitted payment
    /// was rejected (wrong network, expired, double-spend, etc.).
    PaymentRejected {
        reason: String,
        retryable: bool,
        advertised_challenges: DecodedPaymentChallenges,
        resource_url: String,
    },
    /// The command completed (any status other than 402).
    Completed {
        exit_code: i32,
        /// Response body (only set by the built-in fetch, not by curl/wget
        /// wrappers). Held as raw bytes so binary responses round-trip
        /// without UTF-8 mangling. Use [`String::from_utf8_lossy`] to get
        /// a text view when the content-type guarantees UTF-8 (e.g. JSON).
        body: Option<Vec<u8>>,
        /// `content-type` header value (when known). Set by the built-in
        /// fetch path; `None` for the external curl/wget/httpie wrappers.
        /// Lets consumers route
        /// binary responses (image/*, application/pdf, …) differently
        /// from text — see `pay-mcp`'s curl tool.
        content_type: Option<String>,
        /// Final response headers, retained so callers can decode settlement
        /// receipts after a paid retry.
        response_headers: Vec<(String, String)>,
    },
}

/// Run `curl` with the given user args, detecting 402 + MPP challenges.
///
/// Appends `-D <tempfile>` after user args to capture response headers.
/// stdout/stderr/stdin are inherited so the user sees normal curl output.
///
/// `network_override` and `account_override` are the signing context the
/// caller selected; a cached subscription bearer is only pre-attached when
/// it belongs to that context.
pub fn run_curl(
    user_args: &[String],
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> Result<RunOutcome> {
    if is_passthrough_metadata_request(user_args) {
        return run_plain_command("curl", user_args);
    }

    validate_curl_args_against_catalog(user_args)?;
    run_initial(
        ToolKind::Curl,
        user_args,
        network_override,
        account_override,
    )
}

/// Run `curl` with extra headers injected (used for retry after payment).
///
/// `resource_url` is the URL of the 402 the headers answer (the outcome's
/// `resource_url`); the request goes straight there. See [`run_paid`].
pub fn run_curl_with_headers(
    user_args: &[String],
    resource_url: &str,
    extra_headers: &[String],
) -> Result<RunOutcome> {
    run_paid(ToolKind::Curl, user_args, resource_url, extra_headers)
}

/// Validate a curl invocation before Pay attaches or signs anything for it:
/// it must name a single transfer (see [`ensure_single_transfer`]) and match
/// cached Pay catalog OpenAPI metadata.
pub fn validate_curl_args_against_catalog(user_args: &[String]) -> Result<()> {
    ensure_single_transfer(user_args, ToolKind::Curl)?;
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

#[cfg(test)]
fn run_curl_inner(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    let named = resolve_request_url(user_args, ToolKind::Curl).unwrap_or_default();
    run_with_headers(ToolKind::Curl, user_args, extra_headers, &named)
}

fn curl_hop(user_args: &[String], extra_headers: &[String], named: &str) -> Result<Hop> {
    check_command_exists("curl")?;
    if !extra_headers.is_empty() {
        ensure_single_transfer(user_args, ToolKind::Curl)?;
    }

    let header_file = NamedTempFile::new()?;
    let header_path = header_file.path();
    let body_file = NamedTempFile::new()?;
    let body_path = body_file.path();

    let headers = headers_with_default_user_agent(user_args, extra_headers, ToolKind::Curl);

    log_invocation(ToolKind::Curl, user_args, &headers);

    // Body goes to `-o body_file` so we can swallow it on 402; stdout is piped
    // so curl's `-w` writeout (which it emits to stdout after the transfer) is
    // captured and re-emitted on the success path. Without this, `pay curl -w
    // '%{http_code}'` silently drops the writeout because we'd discard stdout.
    let mut cmd = Command::new("curl");
    cmd.args(user_args);
    for h in &headers {
        cmd.arg("-H").arg(h);
    }
    cmd.args(credential_redirect_guard(ToolKind::Curl, extra_headers));
    cmd.arg("-D").arg(header_path);
    cmd.arg("-o").arg(body_path);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output()?;
    let exit_code = output.status.code().unwrap_or(1);
    let headers_raw = std::fs::read_to_string(header_path).unwrap_or_default();
    // Read body as raw bytes — `read_to_string` is lossy on non-UTF-8 and
    // would silently mangle binary responses (images, PDFs, …) before we
    // ever print them.
    let body = std::fs::read(body_path).unwrap_or_default();
    let chain = parse_http_header_blocks(&headers_raw);
    let last = chain.last().cloned().unwrap_or_default();
    let status_code = last.status;

    debug!(?status_code, exit_code, "curl finished");

    if status_code == Some(402) {
        // Swallow stderr/stdout/body on 402 — CLI handles display.
        // 402 challenge bodies are JSON per spec; lossy decode is fine.
        let body_text = String::from_utf8_lossy(&body);
        let url = payment_url(user_args, ToolKind::Curl, &chain, named)?;
        return Ok(Hop::Done(classify_402(
            &last.headers,
            Some(&body_text),
            &url,
        )));
    }
    if !extra_headers.is_empty()
        && let Some(next) = credential_free_redirect(user_args, ToolKind::Curl, &last)
    {
        return Ok(Hop::Redirect(next));
    }
    let headers = last.headers;

    // Not 402 — re-emit stderr (progress bar etc.), body, then any -w writeout.
    // `write_all(&body)` so binary bytes pass through untouched; print!
    // would route through Display which goes through UTF-8.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    let _ = std::io::Write::write_all(&mut std::io::stdout(), &body);
    let writeout = String::from_utf8_lossy(&output.stdout);
    if !writeout.is_empty() {
        print!("{writeout}");
    }
    Ok(Hop::Done(RunOutcome::Completed {
        exit_code,
        body: None,
        content_type: None,
        response_headers: headers,
    }))
}

/// Run `wget` with the given user args, detecting 402 + MPP challenges.
///
/// See [`run_curl`] for the signing-context arguments.
pub fn run_wget(
    user_args: &[String],
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> Result<RunOutcome> {
    if is_passthrough_metadata_request(user_args) {
        return run_plain_command("wget", user_args);
    }

    validate_wget_args_against_catalog(user_args)?;
    run_initial(
        ToolKind::Wget,
        user_args,
        network_override,
        account_override,
    )
}

/// Run `http` (HTTPie) with the given user args, detecting 402 + MPP challenges.
///
/// HTTPie uses positional `Header:Value` request items rather than `-H` flags,
/// so `extra_headers` for retry are appended as positional args.
///
/// See [`run_curl`] for the signing-context arguments.
pub fn run_httpie(
    user_args: &[String],
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> Result<RunOutcome> {
    if is_passthrough_metadata_request(user_args) {
        return run_plain_command("http", user_args);
    }

    run_initial(
        ToolKind::Httpie,
        user_args,
        network_override,
        account_override,
    )
}

/// Run `http` with extra headers injected (used for retry after payment).
///
/// Each entry in `extra_headers` is the literal HTTPie request item
/// (e.g. `"Authorization:Bearer …"`), already formatted by the caller.
pub fn run_httpie_with_headers(
    user_args: &[String],
    resource_url: &str,
    extra_headers: &[String],
) -> Result<RunOutcome> {
    run_paid(ToolKind::Httpie, user_args, resource_url, extra_headers)
}

/// Run `wget` with extra headers injected (used for retry after payment).
/// See [`run_curl_with_headers`].
pub fn run_wget_with_headers(
    user_args: &[String],
    resource_url: &str,
    extra_headers: &[String],
) -> Result<RunOutcome> {
    run_paid(ToolKind::Wget, user_args, resource_url, extra_headers)
}

/// Redirect hops Pay steps itself for a credential-bearing request before
/// giving up, matching the built-in fetch's limit.
const MAX_CREDENTIAL_REDIRECTS: usize = 10;

/// One run of a wrapped tool.
enum Hop {
    Done(RunOutcome),
    /// The tool ran with Pay headers and redirects off, and the response
    /// is a redirect the caller asked to follow: these are the args for the
    /// next hop. Nothing of this response was shown to the caller.
    Redirect(Vec<String>),
}

fn run_hop(tool: ToolKind, args: &[String], extra_headers: &[String], named: &str) -> Result<Hop> {
    match tool {
        ToolKind::Curl => curl_hop(args, extra_headers, named),
        ToolKind::Wget => wget_hop(args, extra_headers, named),
        ToolKind::Httpie => httpie_hop(args, extra_headers, named),
    }
}

/// The first request of a `pay curl|wget|http` command.
///
/// A cached subscription bearer is attached per destination: each redirect
/// hop the caller asked to follow is a new request, and it carries only the
/// bearer cached for its own URL (if any). Hops without one run with the
/// tool's own redirect handling.
fn run_initial(
    tool: ToolKind,
    user_args: &[String],
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> Result<RunOutcome> {
    let named = resolve_request_url(user_args, tool).unwrap_or_default();
    let mut args = user_args.to_vec();
    for _ in 0..=MAX_CREDENTIAL_REDIRECTS {
        let pre = pre_attach_cached_auth(&args, tool, network_override, account_override);
        match run_hop(tool, &args, &pre, &named)? {
            Hop::Done(outcome) => return Ok(outcome),
            Hop::Redirect(next) => args = next,
        }
    }
    Err(too_many_redirects(&named))
}

/// The paid retry: the request with Pay's headers, sent to `resource_url`.
///
/// The headers go to `resource_url` only. If it answers with a redirect the
/// caller asked to follow (`curl -L`, wget's default, `http --follow`) and
/// the request is a bodyless read, the next hop runs without any Pay header,
/// with the tool's own redirect handling from there on.
fn run_paid(
    tool: ToolKind,
    user_args: &[String],
    resource_url: &str,
    extra_headers: &[String],
) -> Result<RunOutcome> {
    let args = paid_request_args(user_args, tool, resource_url);
    let named = resolve_request_url(&args, tool).unwrap_or_default();
    run_with_headers(tool, &args, extra_headers, &named)
}

fn run_with_headers(
    tool: ToolKind,
    args: &[String],
    extra_headers: &[String],
    named: &str,
) -> Result<RunOutcome> {
    match run_hop(tool, args, extra_headers, named)? {
        Hop::Done(outcome) => Ok(outcome),
        Hop::Redirect(next) => match run_hop(tool, &next, &[], named)? {
            Hop::Done(outcome) => Ok(outcome),
            // A hop without Pay headers runs with the tool's own redirect
            // handling and never stops at a redirect.
            Hop::Redirect(_) => Err(too_many_redirects(named)),
        },
    }
}

fn too_many_redirects(url: &str) -> Error {
    Error::RequestValidation(format!("{url} redirected too many times"))
}

/// Validate a wget invocation before Pay attaches or signs anything for it:
/// it must name a single transfer (see [`ensure_single_transfer`]) and match
/// cached Pay catalog OpenAPI metadata.
pub fn validate_wget_args_against_catalog(user_args: &[String]) -> Result<()> {
    ensure_single_transfer(user_args, ToolKind::Wget)?;
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

#[cfg(test)]
fn run_wget_inner(user_args: &[String], extra_headers: &[String]) -> Result<RunOutcome> {
    let named = resolve_request_url(user_args, ToolKind::Wget).unwrap_or_default();
    run_with_headers(ToolKind::Wget, user_args, extra_headers, &named)
}

fn wget_hop(user_args: &[String], extra_headers: &[String], named: &str) -> Result<Hop> {
    check_command_exists("wget")?;
    if !extra_headers.is_empty() {
        ensure_single_transfer(user_args, ToolKind::Wget)?;
    }

    let has_server_response = user_args
        .iter()
        .any(|a| a == "-S" || a == "--server-response");

    let headers = headers_with_default_user_agent(user_args, extra_headers, ToolKind::Wget);

    let mut cmd = Command::new("wget");
    if !has_server_response {
        cmd.arg("--server-response");
    }
    cmd.args(user_args);
    for h in &headers {
        cmd.arg("--header").arg(h);
    }
    cmd.args(credential_redirect_guard(ToolKind::Wget, extra_headers));
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::piped());

    log_invocation(ToolKind::Wget, user_args, &headers);

    let output = cmd.output()?;
    let exit_code = output.status.code().unwrap_or(1);
    let stderr_text = String::from_utf8_lossy(&output.stderr);

    let chain = parse_wget_header_blocks(&stderr_text);
    let last = chain.last().cloned().unwrap_or_default();
    let status_code = last.status;

    debug!(?status_code, exit_code, "wget finished");

    if status_code == Some(402) {
        // Swallow stderr on 402. NOTE: wget writes the body to a file in cwd
        // by default, which we don't want to clobber by injecting -O. As a
        // result, we can't surface server `verification_failed` reasons for
        // wget retries (only curl/fetch). The retry path falls back to a
        // generic "still 402" message.
        let url = payment_url(user_args, ToolKind::Wget, &chain, named)?;
        return Ok(Hop::Done(classify_402(&last.headers, None, &url)));
    }
    // wget writes nothing for a redirect it does not follow, so dropping
    // this run's stderr is all it takes to hide the hop.
    if !extra_headers.is_empty()
        && let Some(next) = credential_free_redirect(user_args, ToolKind::Wget, &last)
    {
        return Ok(Hop::Redirect(next));
    }
    let headers = last.headers;

    // Re-emit stderr on success
    eprint!("{stderr_text}");
    Ok(Hop::Done(RunOutcome::Completed {
        exit_code,
        body: None,
        content_type: None,
        response_headers: headers,
    }))
}

fn httpie_hop(user_args: &[String], extra_headers: &[String], named: &str) -> Result<Hop> {
    use std::io::IsTerminal;

    check_command_exists("http")?;

    let headers = headers_with_default_user_agent(user_args, extra_headers, ToolKind::Httpie);

    log_invocation(ToolKind::Httpie, user_args, &headers);

    // HTTPie has no `-D <file>` equivalent, so we capture stdout and parse the
    // response status from the first `HTTP/x.y <code>` line. We force three flags
    // *after* the user's args so they always win:
    //   - `--no-all` — print only the final exchange. `-v` implies `--all`, whose
    //     combined history cannot be distinguished safely from response body.
    //   - `--print=hb` — httpie's default when piped is body-only, which would
    //     hide the status line our parser needs.
    //   - `--pretty=all` — only when our parent stdout is a TTY, so the user
    //     sees colors despite our pipe (httpie would otherwise auto-disable
    //     them). We strip ANSI codes for parsing.
    let stdout_is_tty = std::io::stdout().is_terminal();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let mut cmd = Command::new("http");
    cmd.args(user_args);
    for h in &headers {
        cmd.arg(h);
    }
    cmd.args(credential_redirect_guard(ToolKind::Httpie, extra_headers));
    cmd.arg("--no-all");
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

    let (status_code, headers, body) = parse_httpie_output(&stdout_text);

    debug!(?status_code, exit_code, "httpie finished");

    // `--no-all` hides the hops HTTPie followed, so the 402 is taken to
    // come from the URL on the command line.
    let last = ResponseHead {
        status: status_code,
        headers,
    };
    if status_code == Some(402) {
        // Swallow stdout/stderr on 402 — CLI handles display
        let url = payment_url(
            user_args,
            ToolKind::Httpie,
            std::slice::from_ref(&last),
            named,
        )?;
        return Ok(Hop::Done(classify_402(
            &last.headers,
            body.as_deref(),
            &url,
        )));
    }
    if !extra_headers.is_empty()
        && let Some(next) = credential_free_redirect(user_args, ToolKind::Httpie, &last)
    {
        return Ok(Hop::Redirect(next));
    }
    let headers = last.headers;

    if !stderr_text.is_empty() {
        eprint!("{stderr_text}");
    }
    print!("{stdout_text}");
    Ok(Hop::Done(RunOutcome::Completed {
        exit_code,
        body: None,
        content_type: None,
        response_headers: headers,
    }))
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
/// The runner forces `--no-all --print=hb`, so the first line beginning with
/// `HTTP/` is the final response status. Once its header block ends, all
/// remaining lines are body. In particular, an `HTTP/` line in the body must
/// never be interpreted as another response. The parser also tolerates a
/// verbose request prelude for callers that exercise it directly.
/// ANSI escapes (from `--pretty=all`) are stripped before parsing.
pub(crate) fn parse_httpie_output(
    raw: &str,
) -> (Option<u16>, Vec<(String, String)>, Option<String>) {
    let cleaned = strip_ansi(raw);
    let lines: Vec<&str> = cleaned.lines().collect();

    let mut status_code = None;
    let mut headers = Vec::new();
    let mut body_start = None;
    let mut in_response_headers = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed_full = line.trim();

        if status_code.is_none() && trimmed_full.starts_with("HTTP/") {
            let parsed_status = trimmed_full
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok());
            if parsed_status.is_some() {
                status_code = parsed_status;
                in_response_headers = true;
            }
            continue;
        }

        if in_response_headers {
            if trimmed_full.is_empty() {
                body_start = Some(i + 1);
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

    let body = body_start.map(|start| lines[start..].join("\n"));
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

#[derive(Clone, Copy)]
enum RawMessageSupport {
    Supported,
    TransactionsOnly(&'static str),
}

impl RawMessageSupport {
    fn from_backend(backend: &dyn crate::backend::SigningBackend) -> Self {
        if backend.signs_raw_messages() {
            Self::Supported
        } else {
            Self::TransactionsOnly(backend.display_name())
        }
    }

    fn from_capability(signs_raw_messages: bool) -> Self {
        if signs_raw_messages {
            Self::Supported
        } else {
            Self::TransactionsOnly("The selected signer")
        }
    }

    fn require(self, what: &str) -> Result<()> {
        match self {
            Self::Supported => Ok(()),
            Self::TransactionsOnly(name) => Err(Error::Config(format!(
                "{name} cannot sign {what}: it needs a raw message signature, but \
                 this signer only signs transactions. Pay with a charge or a client-signed \
                 session instead, or use a software or remote wallet for this service."
            ))),
        }
    }

    fn check_session(self, challenge: &mpp::Challenge) -> Result<()> {
        let request = challenge
            .request
            .decode::<pay_kit::mpp::SessionRequest>()
            .map_err(|error| Error::Mpp(format!("invalid MPP session challenge: {error}")))?;
        if request.method_details.voucher_signer
            == Some(pay_kit::mpp::SessionVoucherSigner::Operator)
        {
            self.require("an operator-signed session proof")?;
        }
        Ok(())
    }
}

impl RunOutcome {
    /// A 402 that names a payment pay knows how to make.
    fn is_payment_challenge(&self) -> bool {
        matches!(
            self,
            RunOutcome::MppChallenge { .. }
                | RunOutcome::SessionChallenge { .. }
                | RunOutcome::SubscriptionChallenge { .. }
                | RunOutcome::X402Challenge { .. }
                | RunOutcome::X402UptoChallenge { .. }
                | RunOutcome::X402BatchChallenge { .. }
                | RunOutcome::X402SignInChallenge { .. }
        )
    }

    /// Account-network slug the selected payment offer will sign on.
    ///
    /// This comes from the 402 challenge rather than the currently active
    /// account, which may belong to an unrelated network.
    pub fn payment_network(&self, network_override: Option<&str>) -> Result<Option<String>> {
        let network = match self {
            RunOutcome::MppChallenge { challenge, .. }
            | RunOutcome::SessionChallenge { challenge, .. }
            | RunOutcome::SubscriptionChallenge { challenge, .. } => {
                mpp::challenge_network(challenge, network_override)?
            }
            RunOutcome::X402Challenge { challenge, .. } => {
                x402::challenge_network(challenge, network_override)
            }
            RunOutcome::X402UptoChallenge { challenge, .. } => {
                x402::scheme_network(&challenge.requirements.network, network_override)
            }
            RunOutcome::X402BatchChallenge { challenge, .. } => {
                x402::scheme_network(&challenge.requirements.network, network_override)
            }
            RunOutcome::X402SignInChallenge { challenge, .. } => {
                x402::sign_in_chain(challenge, network_override)?.1
            }
            _ => return Ok(None),
        };
        Ok(Some(network))
    }

    /// Swap a payment offer `backend` cannot sign for the next offer on the
    /// same 402, so the choice follows what the signer can do rather than
    /// failing at signing time.
    ///
    /// Today this matters for offers that need a raw message signature: an
    /// operator-signed session proof and an x402 sign-in. A hardware wallet
    /// signs neither, so it pays the charge advertised beside them. When the
    /// 402 offered nothing else, the refusal becomes a `PaymentRejected`
    /// with the reason. Every other variant passes through unchanged.
    pub fn for_signer(self, backend: &dyn crate::backend::SigningBackend) -> RunOutcome {
        self.for_raw_message_support(RawMessageSupport::from_backend(backend))
    }

    fn for_raw_message_support(self, support: RawMessageSupport) -> RunOutcome {
        match self {
            RunOutcome::SessionChallenge {
                challenge,
                fallback,
                advertised_challenges,
                resource_url,
            } => match support.check_session(&challenge) {
                Ok(()) => RunOutcome::SessionChallenge {
                    challenge,
                    fallback,
                    advertised_challenges,
                    resource_url,
                },
                Err(refusal) => match fallback {
                    Some(offer) => {
                        info!(
                            resource = %resource_url,
                            "Skipping MPP session the signer cannot open; taking the next offer"
                        );
                        *offer
                    }
                    None => rejected_for_signer(refusal, advertised_challenges, resource_url),
                },
            },
            RunOutcome::X402SignInChallenge {
                challenge,
                payment_fallback,
                advertised_challenges,
                resource_url,
            } => match support.require("an x402 sign-in challenge") {
                Ok(()) => RunOutcome::X402SignInChallenge {
                    challenge,
                    payment_fallback,
                    advertised_challenges,
                    resource_url,
                },
                Err(refusal) => match payment_fallback {
                    Some(payment) => {
                        info!(
                            resource = %resource_url,
                            "Skipping x402 sign-in the signer cannot produce; paying instead"
                        );
                        RunOutcome::X402Challenge {
                            challenge: payment,
                            advertised_challenges,
                            resource_url,
                        }
                    }
                    None => rejected_for_signer(refusal, advertised_challenges, resource_url),
                },
            },
            other => other,
        }
    }

    /// Whether the signer configured for this offer can sign raw messages.
    ///
    /// The named or default signer is read from `accounts.yml` without
    /// prompting. `None` means this outcome needs no capability check or a
    /// throwaway software wallet will be created at payment time.
    pub fn configured_signer_support(
        &self,
        store: &dyn crate::accounts::AccountsStore,
        network_override: Option<&str>,
        account_override: Option<&str>,
    ) -> Result<Option<bool>> {
        // Only the offers `for_signer` may veto are looked at. A malformed
        // offer passes through so its own payment path reports the error.
        let network = match self {
            RunOutcome::SessionChallenge { challenge, .. } => {
                let Ok(request) = challenge.request.decode::<pay_kit::mpp::SessionRequest>() else {
                    return Ok(None);
                };
                network_override
                    .map(str::to_string)
                    .unwrap_or(request.method_details.network)
            }
            RunOutcome::X402SignInChallenge { challenge, .. } => {
                match x402::sign_in_chain(challenge, network_override) {
                    Ok((_, network)) => network,
                    Err(_) => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
        crate::signer::raw_message_support_for_network(&network, store, account_override)
    }

    /// Apply a capability previously returned by
    /// [`configured_signer_support`](Self::configured_signer_support).
    pub fn for_signer_support(self, signs_raw_messages: Option<bool>) -> RunOutcome {
        match signs_raw_messages {
            Some(capability) => {
                self.for_raw_message_support(RawMessageSupport::from_capability(capability))
            }
            None => self,
        }
    }
}

/// The signer refused the only offer: report why instead of failing later.
fn rejected_for_signer(
    refusal: Error,
    advertised_challenges: DecodedPaymentChallenges,
    resource_url: String,
) -> RunOutcome {
    let reason = match refusal {
        Error::Config(message) => message,
        other => other.to_string(),
    };
    RunOutcome::PaymentRejected {
        reason: format!("{reason} This endpoint offers no other payment option."),
        retryable: false,
        advertised_challenges,
        resource_url,
    }
}

/// Caller's preference for which payment protocol to use when a 402
/// advertises more than one. `Auto` mirrors the historical defaults
/// (MPP first for one-shot Solana charges, fall back to x402). The
/// `Only*` variants come from the user passing `--mpp` or `--x402` on
/// the CLI (or any wrapper that sets `PAY_PROTOCOL_ENFORCED`) and turn
/// "fall back" into "error" so the operator gets explicit feedback
/// instead of a silent switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolPreference {
    Auto,
    OnlyMpp,
    OnlyX402,
}

impl ProtocolPreference {
    /// Read the preference from `PAY_PROTOCOL_ENFORCED`. The CLI sets
    /// this from `--mpp` / `--x402`; the MCP launchers (`pay claude`,
    /// `pay codex`) forward it so nested invocations stay consistent.
    pub fn from_env() -> Self {
        match std::env::var("PAY_PROTOCOL_ENFORCED").ok().as_deref() {
            Some("mpp") => Self::OnlyMpp,
            Some("x402") => Self::OnlyX402,
            _ => Self::Auto,
        }
    }
}

/// Decode every payment challenge advertised by a 402 response without
/// applying protocol or network selection. This is intentionally separate
/// from [`classify_402`]: verbose diagnostics should show what the server
/// returned, including offers the caller did not choose.
pub fn decode_payment_challenges(
    headers: &[(String, String)],
    body: Option<&str>,
) -> DecodedPaymentChallenges {
    let mpp = mpp::parse_headers(headers)
        .into_iter()
        .map(|challenge| {
            let mut value = serde_json::to_value(&challenge).unwrap_or_else(|_| {
                serde_json::json!({
                    "id": challenge.id,
                    "realm": challenge.realm,
                    "method": challenge.method.as_str(),
                    "intent": challenge.intent.as_str(),
                })
            });
            if let Some(object) = value.as_object_mut() {
                if let Ok(request) = challenge.request.decode_value() {
                    object.insert("request".to_string(), request);
                }
                if let Some(opaque) = challenge.opaque.as_ref()
                    && let Ok(decoded) = opaque.decode_value()
                {
                    object.insert("opaque".to_string(), decoded);
                }
            }
            value
        })
        .collect();

    let mut x402 = Vec::new();
    for (name, value) in headers {
        if (name.eq_ignore_ascii_case(pay_kit::x402::PAYMENT_REQUIRED_HEADER)
            || name.eq_ignore_ascii_case(pay_kit::x402::X402_V1_PAYMENT_REQUIRED_HEADER))
            && let Some(decoded) = decode_json_value(value)
        {
            append_x402_challenges(&mut x402, decoded, true);
        }
    }

    // x402 also permits the payment-required envelope in the response body.
    // Only treat it as such when it self-identifies, so an arbitrary JSON
    // error body is never mislabeled as a payment challenge.
    if x402.is_empty()
        && let Some(decoded) = body.and_then(decode_json_value)
    {
        append_x402_challenges(&mut x402, decoded, false);
    }

    DecodedPaymentChallenges { x402, mpp }
}

fn append_x402_challenges(
    output: &mut Vec<serde_json::Value>,
    decoded: serde_json::Value,
    trusted_header: bool,
) {
    let self_identifies = decoded.get("x402Version").is_some()
        || decoded.get("scheme").is_some()
        || (decoded.get("network").is_some()
            && (decoded.get("amount").is_some() || decoded.get("maxAmountRequired").is_some()));

    if let Some(accepts) = decoded.get("accepts").and_then(serde_json::Value::as_array) {
        if accepts.is_empty() {
            // SIWX-only x402 challenges carry their challenge in `extensions`
            // and legitimately advertise no payment accepts. Preserve the
            // envelope so verbose output still shows that decoded challenge.
            if (trusted_header || self_identifies) && !output.contains(&decoded) {
                output.push(decoded);
            }
            return;
        }
        for challenge in accepts {
            if !output.contains(challenge) {
                output.push(challenge.clone());
            }
        }
        return;
    }

    if (trusted_header || self_identifies) && !output.contains(&decoded) {
        output.push(decoded);
    }
}

fn decode_json_value(raw: &str) -> Option<serde_json::Value> {
    if let Ok(value) = serde_json::from_str(raw) {
        return Some(value);
    }

    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};

    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        if let Ok(bytes) = engine.decode(raw.trim())
            && let Ok(value) = serde_json::from_slice(&bytes)
        {
            return Some(value);
        }
    }
    None
}

/// Given 402 headers (and optional body), determine the payment protocol.
///
/// Reads the user's protocol preference from the environment. Internal
/// callers in tests should prefer [`classify_402_with_preference`] so
/// they don't have to mutate process-global state.
pub fn classify_402(
    headers: &[(String, String)],
    body: Option<&str>,
    resource_url: &str,
) -> RunOutcome {
    classify_402_with_preference(headers, body, resource_url, ProtocolPreference::from_env())
}

/// Variant of [`classify_402`] that takes an explicit
/// [`ProtocolPreference`] rather than reading the env var. The `--mpp`
/// and `--x402` CLI flags map to `OnlyMpp` / `OnlyX402`; if the server
/// only offers the other protocol the function returns
/// `RunOutcome::PaymentRejected` rather than silently switching.
pub(crate) fn classify_402_with_preference(
    headers: &[(String, String)],
    body: Option<&str>,
    resource_url: &str,
    preference: ProtocolPreference,
) -> RunOutcome {
    classify_402_inner(
        headers,
        body,
        resource_url,
        preference,
        SessionOffers::Considered,
    )
}

/// Whether the classifier may pick an MPP session offer. `Ignored` computes
/// the fallback carried by [`RunOutcome::SessionChallenge`]: the same 402 as
/// seen by a client that cannot open a session.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionOffers {
    Considered,
    Ignored,
}

fn classify_402_inner(
    headers: &[(String, String)],
    body: Option<&str>,
    resource_url: &str,
    preference: ProtocolPreference,
    sessions: SessionOffers,
) -> RunOutcome {
    let advertised_challenges = decode_payment_challenges(headers, body);

    // A `verification_failed` body wins over a fresh challenge: it means the
    // server saw our payment header and rejected it. We must surface the
    // reason instead of looping into a second pay-and-retry.
    if let Some((reason, retryable)) = parse_verification_failure(body) {
        info!(resource = resource_url, %reason, "Server rejected payment");
        return RunOutcome::PaymentRejected {
            reason,
            retryable,
            advertised_challenges,
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
            .find(|(k, _)| k.eq_ignore_ascii_case(pay_kit::x402::PAYMENT_REQUIRED_HEADER))
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

    // x402::parse (from pay_kit::x402) only returns Some when a Solana-
    // compatible `accepts` entry exists — it's already a Solana filter.
    // MPP is chain-agnostic at the parse level, so we need to validate
    // the recipient is a valid Solana pubkey.
    // Session MPP: the method field ("solana") indicates chain support.
    // Session requests don't use ChargeRequest so mpp_is_solana doesn't apply.
    if preference != ProtocolPreference::OnlyX402
        && sessions == SessionOffers::Considered
        && let Some(challenge) = mpp_challenges
            .iter()
            .find(|challenge| challenge.intent.as_str() == "session")
    {
        let is_solana_method = challenge.method.as_str() == "solana";
        if is_solana_method {
            debug!(
                resource = resource_url,
                "Detected MPP payment-channel challenge (Solana)"
            );
            let fallback = classify_402_inner(
                headers,
                body,
                resource_url,
                preference,
                SessionOffers::Ignored,
            );
            return RunOutcome::SessionChallenge {
                challenge: Box::new(challenge.clone()),
                fallback: fallback.is_payment_challenge().then(|| Box::new(fallback)),
                advertised_challenges,
                resource_url: resource_url.to_string(),
            };
        }
        // Non-Solana session — fall through to x402 or error.
    }

    // Subscription challenge: checked before generic charge because the
    // intent is more specific. Solana is the only method profile pay
    // implements today, so we filter by both.
    if preference != ProtocolPreference::OnlyX402
        && let Some(challenge) = mpp_challenges
            .iter()
            .find(|c| subscription::is_subscription_challenge(c))
    {
        info!(
            resource = resource_url,
            "Detected MPP subscription challenge (Solana)"
        );
        let authenticate = mpp_challenges
            .iter()
            .find(|c| c.intent.as_str() == "authenticate" && c.method.as_str() == "solana")
            .cloned()
            .map(Box::new);
        return RunOutcome::SubscriptionChallenge {
            challenge: Box::new(challenge.clone()),
            authenticate,
            advertised_challenges,
            resource_url: resource_url.to_string(),
        };
    }

    // Default policy: prefer MPP for one-shot Solana payments (native
    // protocol), fall back to x402. `--mpp` keeps MPP and refuses the
    // x402 fallback; `--x402` skips the MPP selection entirely so we
    // land on x402 even when MPP is also offered.
    let mut charge_challenges: Vec<mpp::Challenge> = mpp_challenges
        .iter()
        .filter(|challenge| pay_kit::mpp::client::is_solana_charge_challenge(challenge))
        .cloned()
        .collect();
    if !charge_challenges.is_empty() && preference != ProtocolPreference::OnlyX402 {
        let challenge = charge_challenges.remove(0);
        info!(resource = resource_url, "Detected MPP challenge (Solana)");
        // An `upto`-only x402 offer also parses leniently as `exact`, so when a
        // real `upto` is present prefer that reading and drop the (bogus) exact
        // alternative — otherwise the selector could pick an `exact` the server
        // never advertised and the payment would be rejected.
        let x402_upto_accepts = x402::parse_upto_accepts(headers, body);
        let x402_alternative =
            if x402_upto_accepts.is_empty() && x402::parse_batch(headers, body).is_none() {
                x402_challenge.clone().map(Box::new)
            } else {
                None
            };
        return RunOutcome::MppChallenge {
            challenge: Box::new(challenge),
            alternatives: charge_challenges,
            // Carry any x402 charge from the same 402 so the balance- and
            // cost-aware selector can settle it when it's cheaper or the only
            // fundable option.
            x402_alternative,
            x402_upto_accepts,
            advertised_challenges,
            resource_url: resource_url.to_string(),
        };
    }

    // Fall back to x402 if it has a Solana path. Skip when the user
    // pinned MPP via `--mpp`.
    if preference != ProtocolPreference::OnlyMpp {
        // Prefer `sign-in-with-x` when the server offers it: a wallet that
        // already holds credits (or has previously paid) should authenticate
        // and spend those rather than pay again. If the same 402 also
        // advertised a payment option, carry it as `payment_fallback` so the
        // caller can still pay when sign-in doesn't grant access (no credits).
        if let Some(siwx) = x402_siwx_challenge {
            info!(
                resource = resource_url,
                "Detected x402 sign-in challenge (preferring credits over payment)"
            );
            return RunOutcome::X402SignInChallenge {
                challenge: Box::new(siwx),
                payment_fallback: x402_challenge.map(Box::new),
                advertised_challenges,
                resource_url: resource_url.to_string(),
            };
        }

        // x402 `upto` (usage-metered) — checked before exact: an upto-only
        // challenge also parses leniently as exact, so prefer the upto reading
        // when the server advertises the `upto` scheme.
        if let Some(upto) = x402::parse_upto(headers, body) {
            debug!(
                resource = resource_url,
                "Detected x402 upto challenge (Solana)"
            );
            return RunOutcome::X402UptoChallenge {
                challenge: Box::new(upto),
                advertised_challenges,
                resource_url: resource_url.to_string(),
            };
        }

        // x402 `batch-settlement` — like `upto`, checked before exact: a
        // batch-only challenge also parses leniently as exact, and paying it
        // with an exact transfer would be rejected by a server that never
        // advertised that scheme.
        if let Some(batch) = x402::parse_batch(headers, body) {
            debug!(
                resource = resource_url,
                "Detected x402 batch-settlement challenge (Solana)"
            );
            return RunOutcome::X402BatchChallenge {
                challenge: Box::new(batch),
                advertised_challenges,
                resource_url: resource_url.to_string(),
            };
        }

        if let Some(challenge) = x402_challenge {
            info!(resource = resource_url, "Detected x402 challenge (Solana)");
            return RunOutcome::X402Challenge {
                challenge: Box::new(challenge),
                advertised_challenges,
                resource_url: resource_url.to_string(),
            };
        }
    }

    // The user forced a protocol via `--mpp` / `--x402` but the server
    // only offers the other one. Surface that explicitly instead of
    // letting the request loop on an UnknownPaymentRequired.
    match preference {
        ProtocolPreference::OnlyMpp
            if x402_challenge.is_some() || x402_siwx_challenge.is_some() =>
        {
            return RunOutcome::PaymentRejected {
                reason: "Server only offers an x402 challenge, but --mpp was requested. \
                         Drop --mpp to settle via x402, or pick a server that advertises MPP."
                    .to_string(),
                retryable: false,
                advertised_challenges,
                resource_url: resource_url.to_string(),
            };
        }
        ProtocolPreference::OnlyX402
            if mpp_challenges
                .iter()
                .any(|challenge| challenge.method.as_str() == "solana") =>
        {
            return RunOutcome::PaymentRejected {
                reason: "Server only offers an MPP challenge, but --x402 was requested. \
                         Drop --x402 to settle via MPP, or pick a server that advertises x402."
                    .to_string(),
                retryable: false,
                advertised_challenges,
                resource_url: resource_url.to_string(),
            };
        }
        _ => {}
    }

    // Neither protocol supports Solana — tell the user clearly.
    if !mpp_challenges.is_empty() {
        return RunOutcome::PaymentRejected {
            reason: "Server requires payment but only accepts non-Solana chains \
                     (e.g. Base/EVM). This endpoint is not compatible with `pay`. \
                     Check if the provider supports Solana USDC."
                .to_string(),
            retryable: false,
            advertised_challenges,
            resource_url: resource_url.to_string(),
        };
    }

    RunOutcome::UnknownPaymentRequired {
        headers: headers.to_vec(),
        advertised_challenges,
        resource_url: resource_url.to_string(),
    }
}

/// Pure parser: pulls a payment rejection reason out of a 402 JSON body.
///
/// Returns `(message, retryable)` if the body matches the shape emitted by
/// the server emits for verification and session failures:
///
/// ```json
/// {"error": "session_failed", "message": "...", "retryable": true}
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
    let error = v.get("error")?.as_str()?;
    if !matches!(error, "verification_failed" | "session_failed") {
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

/// One response head (status line and headers) out of a tool's trace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ResponseHead {
    status: Option<u16>,
    headers: Vec<(String, String)>,
}

impl ResponseHead {
    #[cfg(test)]
    fn into_parts(self) -> (Option<u16>, Vec<(String, String)>) {
        (self.status, self.headers)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn redirect_location(&self) -> Option<&str> {
        self.status
            .filter(|status| (300..400).contains(status))
            .and_then(|_| self.header("location"))
    }
}

/// Parse HTTP headers from curl's `-D` dump format.
///
/// Handles redirect chains by taking the LAST header block (the final response).
#[cfg(test)]
fn parse_http_headers(raw: &str) -> (Option<u16>, Vec<(String, String)>) {
    parse_http_header_blocks(raw)
        .pop()
        .map_or((None, Vec::new()), ResponseHead::into_parts)
}

/// Every response head in curl's `-D` dump, in the order curl received them
/// (one per redirect hop when curl followed redirects).
fn parse_http_header_blocks(raw: &str) -> Vec<ResponseHead> {
    raw.split("\r\n\r\n")
        .filter(|b| !b.trim().is_empty())
        .map(|block| {
            let mut status = None;
            let mut headers = Vec::new();
            for line in block.lines() {
                let line = line.trim();
                if line.starts_with("HTTP/") {
                    status = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|s| s.parse::<u16>().ok());
                } else if let Some((key, value)) = line.split_once(':') {
                    headers.push((key.trim().to_lowercase(), value.trim().to_string()));
                }
            }
            ResponseHead { status, headers }
        })
        .collect()
}

/// Parse HTTP headers from wget's `--server-response` stderr output.
#[cfg(test)]
fn parse_wget_headers(stderr: &str) -> (Option<u16>, Vec<(String, String)>) {
    parse_wget_header_blocks(stderr)
        .pop()
        .map_or((None, Vec::new()), ResponseHead::into_parts)
}

/// Every response head in wget's `--server-response` output, one per hop.
fn parse_wget_header_blocks(stderr: &str) -> Vec<ResponseHead> {
    let mut blocks: Vec<ResponseHead> = Vec::new();
    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("HTTP/") {
            blocks.push(ResponseHead {
                status: trimmed
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u16>().ok()),
                headers: Vec::new(),
            });
        } else if let Some(block) = blocks.last_mut()
            && let Some((key, value)) = trimmed.split_once(':')
        {
            let key = key.trim();
            if !key.is_empty() && !key.contains(' ') {
                block
                    .headers
                    .push((key.to_lowercase(), value.trim().to_string()));
            }
        }
    }
    blocks
}

/// The URL that produced the last response in `chain`, given the URL the
/// tool was asked for: each redirect hop the tool followed moves it to the
/// hop's `Location`, resolved against the URL that sent it.
fn answering_url(requested: &str, chain: &[ResponseHead]) -> String {
    let Ok(mut current) = url::Url::parse(requested) else {
        return requested.to_string();
    };
    let Some((_last, followed)) = chain.split_last() else {
        return requested.to_string();
    };
    let mut moved = false;
    for head in followed {
        if let Some(next) = head
            .redirect_location()
            .and_then(|location| current.join(location).ok())
        {
            moved |= next != current;
            current = next;
        }
    }
    if moved {
        current.into()
    } else {
        requested.to_string()
    }
}

/// The URL Pay pays for when a 402 arrives at the end of `chain`.
///
/// Without injected headers the tool follows redirects the caller asked for
/// (`curl -L`, wget's default), so the 402 may come from a later hop: a
/// trailing-slash redirect or an `http` to `https` upgrade. The credential is
/// built for, and later sent straight to, the URL that asked for it, so it
/// never rides a redirect. A 402 from another origin than the one the caller
/// named is refused: Pay only pays the origin on the command line.
///
/// The paid retry replays the caller's args at that URL, so it must be the
/// request that got the 402. See [`ensure_replays_the_redirected_request`].
fn payment_url(
    args: &[String],
    tool: ToolKind,
    chain: &[ResponseHead],
    named: &str,
) -> Result<String> {
    // Expands HTTPie's `:port/path` shorthand too, so the URL kept with an
    // activated subscription matches what the next request hits.
    let requested = resolve_request_url(args, tool).unwrap_or_default();
    let answering = answering_url(&requested, chain);
    if answering != requested {
        ensure_replays_the_redirected_request(args, tool, chain, &answering)?;
    }
    if answering == named {
        return Ok(answering);
    }
    let same_origin = match (url::Url::parse(named), url::Url::parse(&answering)) {
        (Ok(from), Ok(to)) => crate::client::fetch::is_same_origin_redirect(&from, &to),
        _ => false,
    };
    if !same_origin {
        return Err(Error::RequestValidation(format!(
            "{named} redirected to {answering}, which asked for payment. Pay only pays \
             the origin named on the command line; request {answering} directly."
        )));
    }
    Ok(answering)
}

/// Refuse, before anything is signed, a paid retry that would not be the
/// request the 402 answered.
///
/// The retry sends the caller's args, method and body included, straight to
/// the URL that asked for payment. After a redirect the tool may have reached
/// that URL with another request: curl and wget turn a POST into a bodyless
/// GET on 301, 302 and 303 (HEAD stays HEAD), and keep the method and body
/// only on 307 and 308. So the replay is faithful for a bodyless GET or HEAD
/// after any redirect, and for any other request only when every hop was a
/// 307 or 308 and the body can be sent again (it is not read from stdin).
/// `-X`, `--post30x` and uploads all fall in the refused set unless every
/// hop kept the method anyway.
fn ensure_replays_the_redirected_request(
    args: &[String],
    tool: ToolKind,
    chain: &[ResponseHead],
    answering: &str,
) -> Result<()> {
    if is_bodyless_read(args, tool) {
        return Ok(());
    }
    let followed = chain.split_last().map_or(&[][..], |(_, followed)| followed);
    let keeps_request = followed
        .iter()
        .filter(|head| head.redirect_location().is_some())
        .all(|head| matches!(head.status, Some(307 | 308)));
    if keeps_request && !reads_body_from_stdin(args, tool) {
        return Ok(());
    }
    Err(Error::RequestValidation(format!(
        "the request was redirected to {answering}, which asked for payment, but the \
         redirect changed the request (its method or body), so Pay cannot replay it \
         faithfully with the payment. Send the request to {answering} directly."
    )))
}

/// Whether the request body (or upload) is read from stdin, which a second
/// run of the tool cannot read again.
fn reads_body_from_stdin(args: &[String], tool: ToolKind) -> bool {
    let Some(spec) = transfer_options(tool) else {
        return false;
    };
    let line = CommandLine::parse(args, spec);
    line.options.iter().any(|option| {
        let value = option.value.unwrap_or_default();
        match (tool, option.name) {
            (ToolKind::Curl, OptionName::Short('T') | OptionName::Long("upload-file")) => {
                matches!(value, "-" | ".")
            }
            // `--data-raw` and `--form-string` are always literal.
            (
                ToolKind::Curl,
                OptionName::Short('d')
                | OptionName::Long("data" | "data-ascii" | "data-binary" | "json"),
            ) => value == "@-",
            (ToolKind::Curl, OptionName::Long("data-urlencode")) => {
                curl_urlencode_reads_stdin(value)
            }
            (ToolKind::Curl, OptionName::Short('F') | OptionName::Long("form")) => {
                curl_form_reads_stdin(value)
            }
            (ToolKind::Wget, OptionName::Long("post-file" | "body-file")) => value == "-",
            _ => false,
        }
    })
}

/// `--data-urlencode` reads a file when an `@` comes before any `=`
/// (`@file`, `name@file`); the file `-` is stdin.
fn curl_urlencode_reads_stdin(value: &str) -> bool {
    match value.find(['=', '@']) {
        Some(at) if value[at..].starts_with('@') => &value[at + 1..] == "-",
        _ => false,
    }
}

/// `-F name=@file` uploads and `-F name=<file` inlines a file, optionally
/// followed by `;type=...` parameters; the file `-` is stdin.
fn curl_form_reads_stdin(value: &str) -> bool {
    let Some((_, content)) = value.split_once('=') else {
        return false;
    };
    let Some(file) = content.strip_prefix(['@', '<']) else {
        return false;
    };
    file.split(';').next() == Some("-")
}

/// Heuristic: find the URL from command args.
fn find_url_in_args(args: &[String]) -> Option<String> {
    args.iter()
        .find(|a| a.starts_with("http://") || a.starts_with("https://"))
        .cloned()
}

/// Which external HTTP client the runner is feeding. The header
/// injection format and URL-extraction rules differ per tool.
#[derive(Debug, Clone, Copy)]
enum ToolKind {
    Curl,
    Wget,
    Httpie,
}

fn headers_with_default_user_agent(
    args: &[String],
    extra_headers: &[String],
    tool: ToolKind,
) -> Vec<String> {
    let mut headers = Vec::with_capacity(extra_headers.len() + 1);
    if !user_already_set_user_agent(args, tool)
        && !extra_headers
            .iter()
            .any(|header| is_header_name(header, "user-agent"))
    {
        headers.push(formatted_header(
            tool,
            "User-Agent",
            &ClientApp::Cli.user_agent(),
        ));
    }
    headers.extend(extra_headers.iter().cloned());
    headers
}

/// Reject a curl or wget invocation that would make more than one transfer.
///
/// Headers Pay injects (a payment, a sign-in proof, a cached subscription
/// bearer) and the caller's shared body apply to every transfer in the
/// command, while the 402 and its credential belong to one URL. A second
/// URL, `--next`, a config or input file, URL globbing, variable expansion
/// or a recursive crawl would hand them to destinations nobody paid. Unknown
/// options are read as flags, so a value they take counts as another URL:
/// the check errs toward refusing the command.
fn ensure_single_transfer(args: &[String], tool: ToolKind) -> Result<()> {
    let refuse = |why: &str| {
        Err(Error::RequestValidation(format!(
            "pay sends payment credentials to a single URL, but this command {why}. \
             Run one `pay {}` per URL.",
            match tool {
                ToolKind::Curl => "curl",
                ToolKind::Wget => "wget",
                ToolKind::Httpie => "http",
            }
        )))
    };
    let Some(spec) = transfer_options(tool) else {
        // HTTPie takes exactly one URL by construction.
        return Ok(());
    };
    let line = CommandLine::parse(args, spec);
    for option in &line.options {
        match option.name {
            OptionName::Long(name) if spec.adds_transfers(name) => {
                return refuse(&format!("uses `--{name}`"));
            }
            // The tool itself rejects an ambiguous abbreviation, but one
            // that could stand for a transfer-adding option is refused here
            // whatever the tool's option table says.
            OptionName::Ambiguous(typed)
                if spec
                    .long
                    .candidates(typed)
                    .any(|name| spec.adds_transfers(name)) =>
            {
                return refuse(&format!("uses `--{typed}`"));
            }
            OptionName::Short(c) if spec.multi_transfer_short.contains(c) => {
                return refuse(&format!("uses `-{c}`"));
            }
            _ => {}
        }
    }
    for target in &line.targets {
        if target.via_option && target.value.starts_with('@') {
            return refuse("reads URLs from a file");
        }
    }
    if line.targets.len() > 1 {
        return refuse(&format!("names {} URLs", line.targets.len()));
    }
    Ok(())
}

fn transfer_options(tool: ToolKind) -> Option<&'static TransferOptions> {
    match tool {
        ToolKind::Curl => Some(&CURL_OPTIONS),
        ToolKind::Wget => Some(&WGET_OPTIONS),
        ToolKind::Httpie => None,
    }
}

/// A curl or wget command line split the way the tool reads it: options
/// with the value each one consumes, and the transfer targets (URLs).
///
/// Every check over a command line goes through this one reading, so an
/// option is never seen as a flag by one check and as a URL by another.
/// Unknown options are read as flags: a value they take then counts as
/// another target, which errs toward refusing the command.
struct CommandLine<'a> {
    options: Vec<ParsedOption<'a>>,
    targets: Vec<TargetArg<'a>>,
}

struct ParsedOption<'a> {
    name: OptionName<'a>,
    value: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionName<'a> {
    /// A long option by its full name, however the caller abbreviated it.
    Long(&'a str),
    /// An abbreviation the tool refuses because it fits several options.
    Ambiguous(&'a str),
    Short(char),
}

/// Where a transfer target sits in the argument list.
struct TargetArg<'a> {
    index: usize,
    value: &'a str,
    /// The target is the value of the tool's URL option (`--url X` or
    /// `--url=X`) rather than a positional argument.
    via_option: bool,
    /// `args[index]` is `--url=<value>`, not the bare value.
    inline: bool,
}

impl<'a> CommandLine<'a> {
    fn parse(args: &'a [String], spec: &TransferOptions) -> Self {
        let mut options = Vec::new();
        let mut targets = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            if arg == "--" {
                targets.extend((i + 1..args.len()).map(|index| TargetArg {
                    index,
                    value: args[index].as_str(),
                    via_option: false,
                    inline: false,
                }));
                break;
            }
            if let Some(long) = arg.strip_prefix("--") {
                let (name, inline_value) = match long.split_once('=') {
                    Some((name, value)) => (name, Some(value)),
                    None => (long, None),
                };
                let (name, arg) = match spec.long.resolve(name) {
                    LongMatch::Known(name, arg) => (OptionName::Long(name), arg),
                    LongMatch::Ambiguous(typed) => (OptionName::Ambiguous(typed), LongArg::None),
                };
                let is_url = spec
                    .url_long
                    .is_some_and(|url| name == OptionName::Long(url));
                let takes_value = is_url || arg == LongArg::Required;
                let value = match inline_value {
                    Some(value) => Some(value),
                    None if takes_value => {
                        i += 1;
                        args.get(i).map(String::as_str)
                    }
                    None => None,
                };
                if is_url && let Some(value) = value {
                    targets.push(TargetArg {
                        index: i,
                        value,
                        via_option: true,
                        inline: inline_value.is_some(),
                    });
                }
                options.push(ParsedOption { name, value });
            } else if let Some(shorts) = arg.strip_prefix('-').filter(|s| !s.is_empty()) {
                let mut chars = shorts.char_indices();
                while let Some((at, c)) = chars.next() {
                    if spec.boolean_prefix_short.contains(c) {
                        // wget's `-nv`, `-nc`, ...: the next letter is part of
                        // the flag name, not another option.
                        chars.next();
                        continue;
                    }
                    if spec.value_short.contains(c) {
                        let rest = &shorts[at + c.len_utf8()..];
                        let value = if rest.is_empty() {
                            i += 1;
                            args.get(i).map(String::as_str)
                        } else {
                            Some(rest)
                        };
                        options.push(ParsedOption {
                            name: OptionName::Short(c),
                            value,
                        });
                        break;
                    }
                    options.push(ParsedOption {
                        name: OptionName::Short(c),
                        value: None,
                    });
                }
            } else {
                targets.push(TargetArg {
                    index: i,
                    value: arg,
                    via_option: false,
                    inline: false,
                });
            }
            i += 1;
        }
        Self { options, targets }
    }
}

/// The options of a wget command line, by full name.
fn wget_options(args: &[String]) -> impl Iterator<Item = (OptionName<'_>, Option<&str>)> {
    CommandLine::parse(args, &WGET_OPTIONS)
        .options
        .into_iter()
        .map(|option| (option.name, option.value))
}

/// What [`ensure_single_transfer`] needs to know about a tool's options.
struct TransferOptions {
    /// Short options that consume a value (attached or as the next arg).
    value_short: &'static str,
    /// Short letters that start a two-letter boolean flag (wget's `-n?`).
    boolean_prefix_short: &'static str,
    /// Short options that add transfers on their own.
    multi_transfer_short: &'static str,
    /// How long options (without `--`) are spelled and what they take.
    long: LongOptions,
    /// Long options that add transfers on their own.
    multi_transfer_long: &'static [&'static str],
    multi_transfer_long_prefixes: &'static [&'static str],
    /// The long option whose value is a URL, if the tool has one.
    url_long: Option<&'static str>,
}

impl TransferOptions {
    fn adds_transfers(&self, name: &str) -> bool {
        self.multi_transfer_long.contains(&name)
            || self
                .multi_transfer_long_prefixes
                .iter()
                .any(|p| name.starts_with(p))
    }
}

/// Whether a long option takes a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LongArg {
    None,
    /// The value is the rest of the argument or the next one.
    Required,
    /// The value is only ever given inline (`--name=value`).
    Optional,
}

/// How a tool matches the long options on its command line.
enum LongOptions {
    /// Names are matched exactly (curl). Only the ones that take a value
    /// are listed; anything else is a flag.
    Exact {
        takes_value: &'static [&'static str],
    },
    /// `getopt_long` (wget): an exact name, or else any prefix of exactly
    /// one name, selects that option. This is the tool's whole table.
    Abbreviable(&'static [(&'static str, LongArg)]),
}

enum LongMatch<'a> {
    Known(&'a str, LongArg),
    Ambiguous(&'a str),
}

impl LongOptions {
    fn resolve<'a>(&self, typed: &'a str) -> LongMatch<'a> {
        match self {
            Self::Exact { takes_value } => {
                let arg = if takes_value.contains(&typed) {
                    LongArg::Required
                } else {
                    LongArg::None
                };
                LongMatch::Known(typed, arg)
            }
            Self::Abbreviable(table) => {
                if let Some(&(name, arg)) = table.iter().find(|(name, _)| *name == typed) {
                    return LongMatch::Known(name, arg);
                }
                let mut candidates = table.iter().filter(|(name, _)| name.starts_with(typed));
                match (candidates.next(), candidates.next()) {
                    (Some(&(name, arg)), None) => LongMatch::Known(name, arg),
                    (Some(_), Some(_)) => LongMatch::Ambiguous(typed),
                    // Unknown: the tool refuses to run at all.
                    (None, _) => LongMatch::Known(typed, LongArg::None),
                }
            }
        }
    }

    /// Every option name `typed` could abbreviate.
    fn candidates<'s>(&'s self, typed: &'s str) -> impl Iterator<Item = &'static str> + 's {
        let table: &'static [(&'static str, LongArg)] = match self {
            Self::Exact { .. } => &[],
            Self::Abbreviable(table) => table,
        };
        table
            .iter()
            .map(|(name, _)| *name)
            .filter(move |name| name.starts_with(typed))
    }
}

static CURL_OPTIONS: TransferOptions = TransferOptions {
    value_short: "ECbcdDFPHmoxUQreXYytzTuAw",
    boolean_prefix_short: "",
    // `-:` is `--next`, `-K` reads a config file that can add URLs.
    multi_transfer_short: ":K",
    // curl matches long options exactly: `--nex` is an unknown option, not
    // `--next` (checked against curl 8.7 and 8.20).
    long: LongOptions::Exact {
        takes_value: &[
            "abstract-unix-socket",
            "alt-svc",
            "aws-sigv4",
            "cacert",
            "capath",
            "cert",
            "cert-type",
            "ciphers",
            "connect-timeout",
            "connect-to",
            "continue-at",
            "cookie",
            "cookie-jar",
            "create-file-mode",
            "crlfile",
            "curves",
            "data",
            "data-ascii",
            "data-binary",
            "data-raw",
            "data-urlencode",
            "delegation",
            "dns-interface",
            "dns-ipv4-addr",
            "dns-ipv6-addr",
            "dns-servers",
            "doh-url",
            "dump-header",
            "ech",
            "egd-file",
            "engine",
            "etag-compare",
            "etag-save",
            "expect100-timeout",
            "form",
            "form-string",
            "ftp-account",
            "ftp-alternative-to-user",
            "ftp-method",
            "ftp-port",
            "ftp-ssl-ccc-mode",
            "happy-eyeballs-timeout-ms",
            "haproxy-clientip",
            "header",
            "hostpubmd5",
            "hostpubsha256",
            "hsts",
            "interface",
            "ip-tos",
            "ipfs-gateway",
            "json",
            "keepalive-cnt",
            "keepalive-time",
            "key",
            "key-type",
            "knownhosts",
            "krb",
            "libcurl",
            "limit-rate",
            "local-port",
            "login-options",
            "mail-auth",
            "mail-from",
            "mail-rcpt",
            "max-filesize",
            "max-redirs",
            "max-time",
            "netrc-file",
            "noproxy",
            "oauth2-bearer",
            "output",
            "output-dir",
            "parallel-max",
            "pass",
            "pinnedpubkey",
            "preproxy",
            "proto",
            "proto-default",
            "proto-redir",
            "proxy",
            "proxy1.0",
            "proxy-cacert",
            "proxy-capath",
            "proxy-cert",
            "proxy-cert-type",
            "proxy-ciphers",
            "proxy-crlfile",
            "proxy-header",
            "proxy-key",
            "proxy-key-type",
            "proxy-pass",
            "proxy-pinnedpubkey",
            "proxy-service-name",
            "proxy-tls13-ciphers",
            "proxy-tlsauthtype",
            "proxy-tlspassword",
            "proxy-tlsuser",
            "proxy-user",
            "pubkey",
            "quote",
            "random-file",
            "range",
            "rate",
            "referer",
            "request",
            "request-target",
            "resolve",
            "retry",
            "retry-delay",
            "retry-max-time",
            "sasl-authzid",
            "service-name",
            "sigalgs",
            "socks4",
            "socks4a",
            "socks5",
            "socks5-gssapi-service",
            "socks5-hostname",
            "speed-limit",
            "speed-time",
            "ssl-sessions",
            "stderr",
            "telnet-option",
            "tftp-blksize",
            "time-cond",
            "tls-max",
            "tls13-ciphers",
            "tlsauthtype",
            "tlspassword",
            "tlsuser",
            "trace",
            "trace-ascii",
            "trace-config",
            "unix-socket",
            "upload-file",
            "upload-flags",
            "url-query",
            "user",
            "user-agent",
            "vlan-priority",
            "write-out",
        ],
    },
    // `--variable` and `--expand-*` can synthesise URLs at run time.
    multi_transfer_long: &["next", "config", "variable"],
    multi_transfer_long_prefixes: &["expand-"],
    url_long: Some("url"),
};

static WGET_OPTIONS: TransferOptions = TransferOptions {
    value_short: "oaBtOTwQPUlARDIXY",
    boolean_prefix_short: "n",
    // `-i` reads URLs from a file; `-r`, `-m` and `-p` crawl; `-e` runs
    // wgetrc commands, which can set any of those.
    multi_transfer_short: "irmpe",
    long: LongOptions::Abbreviable(WGET_LONG_OPTIONS),
    multi_transfer_long: &[
        "input-file",
        "input-metalink",
        "recursive",
        "mirror",
        "page-requisites",
        "execute",
        "config",
        // Downloads the file from the mirrors a metalink response lists.
        "metalink-over-http",
    ],
    multi_transfer_long_prefixes: &[],
    url_long: None,
};

/// wget's long options, as `getopt_long` sees them: every entry of
/// `option_data` in wget's `src/main.c`, plus the `no-` twin `init_switches`
/// adds for each boolean (booleans take an inline value only, `--foo=off`).
/// The whole table is needed to resolve abbreviations the way wget does:
/// `--input-fil` is `--input-file`, `--mirro` is `--mirror`, `--exec` is
/// `--execute`.
static WGET_LONG_OPTIONS: &[(&str, LongArg)] = &[
    ("accept", LongArg::Required),
    ("accept-regex", LongArg::Required),
    ("adjust-extension", LongArg::Optional),
    ("append-output", LongArg::Required),
    ("ask-password", LongArg::Optional),
    ("auth-no-challenge", LongArg::Optional),
    ("background", LongArg::Optional),
    ("backup-converted", LongArg::Optional),
    ("backups", LongArg::Optional),
    ("base", LongArg::Required),
    ("bind-address", LongArg::Required),
    ("bind-dns-address", LongArg::Required),
    ("body-data", LongArg::Required),
    ("body-file", LongArg::Required),
    ("ca-certificate", LongArg::Required),
    ("ca-directory", LongArg::Required),
    ("cache", LongArg::Optional),
    ("certificate", LongArg::Required),
    ("certificate-type", LongArg::Required),
    ("check-certificate", LongArg::Optional),
    ("ciphers", LongArg::Required),
    ("clobber", LongArg::Optional),
    ("compression", LongArg::Required),
    ("config", LongArg::Required),
    ("connect-timeout", LongArg::Required),
    ("content-disposition", LongArg::Optional),
    ("content-on-error", LongArg::Optional),
    ("continue", LongArg::Optional),
    ("convert-file-only", LongArg::Optional),
    ("convert-links", LongArg::Optional),
    ("cookies", LongArg::Optional),
    ("crl-file", LongArg::Required),
    ("cut-dirs", LongArg::Required),
    ("debug", LongArg::Optional),
    ("default-page", LongArg::Required),
    ("delete-after", LongArg::Optional),
    ("directories", LongArg::Optional),
    ("directory-prefix", LongArg::Required),
    ("dns-cache", LongArg::Optional),
    ("dns-servers", LongArg::Required),
    ("dns-timeout", LongArg::Required),
    ("domains", LongArg::Required),
    ("dont-remove-listing", LongArg::None),
    ("dot-style", LongArg::Required),
    ("egd-file", LongArg::Required),
    ("exclude-directories", LongArg::Required),
    ("exclude-domains", LongArg::Required),
    ("execute", LongArg::Required),
    ("follow-ftp", LongArg::Optional),
    ("follow-tags", LongArg::Required),
    ("force-directories", LongArg::Optional),
    ("force-html", LongArg::Optional),
    ("ftp-password", LongArg::Required),
    ("ftp-stmlf", LongArg::Optional),
    ("ftp-user", LongArg::Required),
    ("ftps-clear-data-connection", LongArg::Optional),
    ("ftps-fallback-to-ftp", LongArg::Optional),
    ("ftps-implicit", LongArg::Optional),
    ("ftps-resume-ssl", LongArg::Optional),
    ("glob", LongArg::Optional),
    ("header", LongArg::Required),
    ("help", LongArg::None),
    ("host-directories", LongArg::Optional),
    ("hsts", LongArg::Optional),
    ("hsts-file", LongArg::Required),
    ("html-extension", LongArg::Optional),
    ("htmlify", LongArg::Optional),
    ("http-keep-alive", LongArg::Optional),
    ("http-passwd", LongArg::Required),
    ("http-password", LongArg::Required),
    ("http-user", LongArg::Required),
    ("https-only", LongArg::Optional),
    ("if-modified-since", LongArg::Optional),
    ("ignore-case", LongArg::Optional),
    ("ignore-length", LongArg::Optional),
    ("ignore-tags", LongArg::Required),
    ("include-directories", LongArg::Required),
    ("inet4-only", LongArg::Optional),
    ("inet6-only", LongArg::Optional),
    ("input-file", LongArg::Required),
    ("input-metalink", LongArg::Required),
    ("iri", LongArg::Optional),
    ("keep-badhash", LongArg::Optional),
    ("keep-session-cookies", LongArg::Optional),
    ("level", LongArg::Required),
    ("limit-rate", LongArg::Required),
    ("load-cookies", LongArg::Required),
    ("local-encoding", LongArg::Required),
    ("max-redirect", LongArg::Required),
    ("metalink-index", LongArg::Required),
    ("metalink-over-http", LongArg::Optional),
    ("method", LongArg::Required),
    ("mirror", LongArg::Optional),
    ("netrc", LongArg::Optional),
    ("no", LongArg::Required),
    ("no-adjust-extension", LongArg::None),
    ("no-ask-password", LongArg::None),
    ("no-auth-no-challenge", LongArg::None),
    ("no-background", LongArg::None),
    ("no-backup-converted", LongArg::None),
    ("no-backups", LongArg::None),
    ("no-cache", LongArg::None),
    ("no-check-certificate", LongArg::None),
    ("no-clobber", LongArg::Optional),
    ("no-config", LongArg::Optional),
    ("no-content-disposition", LongArg::None),
    ("no-content-on-error", LongArg::None),
    ("no-continue", LongArg::None),
    ("no-convert-file-only", LongArg::None),
    ("no-convert-links", LongArg::None),
    ("no-cookies", LongArg::None),
    ("no-debug", LongArg::None),
    ("no-delete-after", LongArg::None),
    ("no-directories", LongArg::None),
    ("no-dns-cache", LongArg::None),
    ("no-follow-ftp", LongArg::None),
    ("no-force-directories", LongArg::None),
    ("no-force-html", LongArg::None),
    ("no-ftp-stmlf", LongArg::None),
    ("no-ftps-clear-data-connection", LongArg::None),
    ("no-ftps-fallback-to-ftp", LongArg::None),
    ("no-ftps-implicit", LongArg::None),
    ("no-ftps-resume-ssl", LongArg::None),
    ("no-glob", LongArg::None),
    ("no-host-directories", LongArg::None),
    ("no-hsts", LongArg::None),
    ("no-html-extension", LongArg::None),
    ("no-htmlify", LongArg::None),
    ("no-http-keep-alive", LongArg::None),
    ("no-https-only", LongArg::None),
    ("no-if-modified-since", LongArg::None),
    ("no-ignore-case", LongArg::None),
    ("no-ignore-length", LongArg::None),
    ("no-inet4-only", LongArg::None),
    ("no-inet6-only", LongArg::None),
    ("no-iri", LongArg::None),
    ("no-keep-badhash", LongArg::None),
    ("no-keep-session-cookies", LongArg::None),
    ("no-metalink-over-http", LongArg::None),
    ("no-mirror", LongArg::None),
    ("no-netrc", LongArg::None),
    ("no-no-clobber", LongArg::None),
    ("no-no-config", LongArg::None),
    ("no-no-parent", LongArg::None),
    ("no-page-requisites", LongArg::None),
    ("no-parent", LongArg::Optional),
    ("no-passive-ftp", LongArg::None),
    ("no-preserve-permissions", LongArg::None),
    ("no-protocol-directories", LongArg::None),
    ("no-proxy", LongArg::None),
    ("no-quiet", LongArg::None),
    ("no-random-wait", LongArg::None),
    ("no-recursive", LongArg::None),
    ("no-relative", LongArg::None),
    ("no-remove-listing", LongArg::None),
    ("no-report-speed", LongArg::None),
    ("no-restrict-file-names", LongArg::None),
    ("no-retr-symlinks", LongArg::None),
    ("no-retry-connrefused", LongArg::None),
    ("no-retry-on-host-error", LongArg::None),
    ("no-save-headers", LongArg::None),
    ("no-server-response", LongArg::None),
    ("no-show-progress", LongArg::None),
    ("no-span-hosts", LongArg::None),
    ("no-spider", LongArg::None),
    ("no-strict-comments", LongArg::None),
    ("no-timestamping", LongArg::None),
    ("no-trust-server-names", LongArg::None),
    ("no-unlink", LongArg::None),
    ("no-use-server-timestamps", LongArg::None),
    ("no-verbose", LongArg::None),
    ("no-warc-cdx", LongArg::None),
    ("no-warc-compression", LongArg::None),
    ("no-warc-digests", LongArg::None),
    ("no-warc-keep-log", LongArg::None),
    ("no-wdebug", LongArg::None),
    ("no-xattr", LongArg::None),
    ("output-document", LongArg::Required),
    ("output-file", LongArg::Required),
    ("page-requisites", LongArg::Optional),
    ("parent", LongArg::Optional),
    ("passive-ftp", LongArg::Optional),
    ("password", LongArg::Required),
    ("pinnedpubkey", LongArg::Required),
    ("post-data", LongArg::Required),
    ("post-file", LongArg::Required),
    ("prefer-family", LongArg::Required),
    ("preferred-location", LongArg::Required),
    ("preserve-permissions", LongArg::Optional),
    ("private-key", LongArg::Required),
    ("private-key-type", LongArg::Required),
    ("progress", LongArg::Required),
    ("protocol-directories", LongArg::Optional),
    ("proxy", LongArg::Optional),
    ("proxy-passwd", LongArg::Required),
    ("proxy-password", LongArg::Required),
    ("proxy-user", LongArg::Required),
    ("proxy__compat", LongArg::Required),
    ("quiet", LongArg::Optional),
    ("quota", LongArg::Required),
    ("random-file", LongArg::Required),
    ("random-wait", LongArg::Optional),
    ("read-timeout", LongArg::Required),
    ("recursive", LongArg::Optional),
    ("referer", LongArg::Required),
    ("regex-type", LongArg::Required),
    ("reject", LongArg::Required),
    ("reject-regex", LongArg::Required),
    ("rejected-log", LongArg::Required),
    ("relative", LongArg::Optional),
    ("remote-encoding", LongArg::Required),
    ("remove-listing", LongArg::Optional),
    ("report-speed", LongArg::Optional),
    ("restrict-file-names", LongArg::Optional),
    ("retr-symlinks", LongArg::Optional),
    ("retry-connrefused", LongArg::Optional),
    ("retry-on-host-error", LongArg::Optional),
    ("retry-on-http-error", LongArg::Required),
    ("save-cookies", LongArg::Required),
    ("save-headers", LongArg::Optional),
    ("secure-protocol", LongArg::Required),
    ("server-response", LongArg::Optional),
    ("show-progress", LongArg::Optional),
    ("span-hosts", LongArg::Optional),
    ("spider", LongArg::Optional),
    ("start-pos", LongArg::Required),
    ("strict-comments", LongArg::Optional),
    ("timeout", LongArg::Required),
    ("timestamping", LongArg::Optional),
    ("tries", LongArg::Required),
    ("trust-server-names", LongArg::Optional),
    ("unlink", LongArg::Optional),
    ("use-askpass", LongArg::Required),
    ("use-server-timestamps", LongArg::Optional),
    ("user", LongArg::Required),
    ("user-agent", LongArg::Required),
    ("verbose", LongArg::Optional),
    ("version", LongArg::None),
    ("wait", LongArg::Required),
    ("waitretry", LongArg::Required),
    ("warc-cdx", LongArg::Optional),
    ("warc-compression", LongArg::Optional),
    ("warc-dedup", LongArg::Required),
    ("warc-digests", LongArg::Optional),
    ("warc-file", LongArg::Required),
    ("warc-header", LongArg::Required),
    ("warc-keep-log", LongArg::Optional),
    ("warc-max-size", LongArg::Required),
    ("warc-tempdir", LongArg::Required),
    ("wdebug", LongArg::Optional),
    ("xattr", LongArg::Optional),
];

/// Flags that stop the wrapped tool from following redirects, appended after
/// the user's args (so they win over `-L`, `--location-trusted`, `--follow`
/// or a `--max-redirect` the caller passed) whenever Pay injects headers.
///
/// Every header Pay injects is a credential bound to the URL the 402 came
/// from: a payment, a sign-in proof, or a cached subscription bearer. The
/// wrapped tools forward custom headers across redirects (wget even does so
/// by default, and curl only strips `Authorization`, not `X-PAYMENT`), so a
/// redirect to another origin would hand the credential to that origin.
/// Not following is the only boundary we can enforce from outside the tool.
/// When the caller asked to follow, Pay takes the next hop itself without
/// its headers (see [`credential_free_redirect`]).
fn credential_redirect_guard(tool: ToolKind, injected: &[String]) -> &'static [&'static str] {
    if injected.is_empty() {
        return &[];
    }
    match tool {
        // `--globoff` keeps `{a,b}` / `[1-9]` in the URL from expanding into
        // more transfers that would carry the same credential.
        ToolKind::Curl => &["--no-location", "--globoff"],
        ToolKind::Wget => &["--max-redirect=0"],
        ToolKind::Httpie => &["--no-follow"],
    }
}

/// Whether the caller's own args ask the tool to follow redirects: `-L`,
/// `--location` or `--location-trusted` for curl, wget's default unless
/// `--max-redirect=0`, `--follow` / `-F` for HTTPie. The last word wins.
fn follows_redirects(args: &[String], tool: ToolKind) -> bool {
    match tool {
        ToolKind::Curl => {
            let line = CommandLine::parse(args, &CURL_OPTIONS);
            line.options
                .iter()
                .fold(false, |follow, option| match option.name {
                    OptionName::Short('L') | OptionName::Long("location" | "location-trusted") => {
                        true
                    }
                    OptionName::Long("no-location") => false,
                    _ => follow,
                })
        }
        ToolKind::Wget => {
            let line = CommandLine::parse(args, &WGET_OPTIONS);
            line.options
                .iter()
                .fold(true, |follow, option| match option.name {
                    OptionName::Long("max-redirect") => {
                        option.value.is_none_or(|v| v.trim() != "0")
                    }
                    _ => follow,
                })
        }
        ToolKind::Httpie => args.iter().fold(false, |follow, arg| match arg.as_str() {
            "--follow" | "-F" => true,
            "--no-follow" => false,
            _ => follow,
        }),
    }
}

/// Whether the request is a bodyless `GET` or `HEAD`, the only kind Pay
/// replays at a redirect target: a body was sent to the URL it was bound to
/// and is not forwarded anywhere else.
fn is_bodyless_read(args: &[String], tool: ToolKind) -> bool {
    let read_method = |method: &str| matches!(method.to_ascii_uppercase().as_str(), "GET" | "HEAD");
    match tool {
        ToolKind::Curl => {
            let line = CommandLine::parse(args, &CURL_OPTIONS);
            line.options.iter().all(|option| match option.name {
                OptionName::Short('d' | 'F' | 'T')
                | OptionName::Long(
                    "data" | "data-ascii" | "data-binary" | "data-raw" | "data-urlencode" | "json"
                    | "form" | "form-string" | "upload-file",
                ) => false,
                OptionName::Short('X') | OptionName::Long("request") => {
                    option.value.is_some_and(read_method)
                }
                _ => true,
            })
        }
        ToolKind::Wget => {
            let line = CommandLine::parse(args, &WGET_OPTIONS);
            line.options.iter().all(|option| match option.name {
                OptionName::Long("post-data" | "post-file" | "body-data" | "body-file") => false,
                OptionName::Long("method") => option.value.is_some_and(read_method),
                _ => true,
            })
        }
        ToolKind::Httpie => {
            let mut positionals = args.iter().filter(|arg| !arg.starts_with('-'));
            let mut first = positionals.next();
            if let Some(method) = first.filter(|m| m.chars().all(|c| c.is_ascii_alphabetic())) {
                if !read_method(method) {
                    return false;
                }
                first = positionals.next();
            }
            // After the URL, only header (`Name:value`) and query
            // (`name==value`) items: anything else is body data.
            first.is_some()
                && positionals.all(|item| {
                    item.contains("==")
                        || item.split_once(':').is_some_and(|(name, value)| {
                            !name.is_empty()
                                && !name.contains('=')
                                && !value.starts_with('=')
                                && !value.starts_with('@')
                        })
                })
                && !args.iter().any(|arg| {
                    matches!(
                        arg.as_str(),
                        "--form" | "-f" | "--multipart" | "--raw" | "--json" | "-j"
                    )
                })
        }
    }
}

/// `args` with the request URL `from` replaced by `to`, or `None` when the
/// URL is not a single argument Pay can locate.
fn retarget_args(args: &[String], tool: ToolKind, from: &str, to: &str) -> Option<Vec<String>> {
    let index = match transfer_options(tool) {
        Some(spec) => {
            let line = CommandLine::parse(args, spec);
            let [target] = line.targets.as_slice() else {
                return None;
            };
            if target.value != from {
                return None;
            }
            let mut retargeted = args.to_vec();
            retargeted[target.index] = if target.inline {
                format!("--url={to}")
            } else {
                to.to_string()
            };
            return Some(retargeted);
        }
        None => args.iter().position(|arg| {
            find_url_in_args(std::slice::from_ref(arg))
                .or_else(|| httpie_url_shorthand(arg))
                .as_deref()
                == Some(from)
        })?,
    };
    let mut retargeted = args.to_vec();
    retargeted[index] = to.to_string();
    Some(retargeted)
}

/// The paid request's args aimed at `resource_url`, the URL whose 402 the
/// credential answers (see [`payment_url`]), so the credential goes straight
/// there instead of to a URL that redirects to it.
fn paid_request_args(args: &[String], tool: ToolKind, resource_url: &str) -> Vec<String> {
    match resolve_request_url(args, tool) {
        Some(requested) if requested != resource_url && !resource_url.is_empty() => {
            retarget_args(args, tool, &requested, resource_url).unwrap_or_else(|| args.to_vec())
        }
        _ => args.to_vec(),
    }
}

/// After a paid request answered with a redirect the caller asked the tool
/// to follow, the args for the next hop, which runs with no Pay header.
///
/// The tool runs credential-bearing requests with redirects off (see
/// [`credential_redirect_guard`]); this is how `pay curl -L` and `pay wget`
/// still reach a resource the paid URL redirects to (a download link, a
/// canonical path) without the payment, sign-in proof or subscription bearer
/// ever leaving the URL it was bound to.
fn credential_free_redirect(
    args: &[String],
    tool: ToolKind,
    response: &ResponseHead,
) -> Option<Vec<String>> {
    let location = response.redirect_location()?;
    if !follows_redirects(args, tool) || !is_bodyless_read(args, tool) {
        return None;
    }
    let requested = resolve_request_url(args, tool)?;
    let next = url::Url::parse(&requested).ok()?.join(location).ok()?;
    if !matches!(next.scheme(), "http" | "https") {
        return None;
    }
    retarget_args(args, tool, &requested, next.as_str())
}

/// Debug-log a wrapped tool invocation without any header value or argument.
///
/// Injected headers are payment credentials, sign-in proofs or subscription
/// bearers, and the caller's own args can hold `-H Authorization: ...`,
/// `-u user:pass` or a request body. A debug filter must never turn into a
/// credential sink, so only header names and the argument count are logged.
fn log_invocation(tool: ToolKind, user_args: &[String], headers: &[String]) {
    let header_names: Vec<&str> = headers
        .iter()
        .map(|header| {
            header
                .split_once(':')
                .map_or("<malformed>", |(name, _)| name.trim())
        })
        .collect();
    debug!(
        tool = ?tool,
        arg_count = user_args.len(),
        headers = ?header_names,
        "Running wrapped HTTP client"
    );
}

fn formatted_header(tool: ToolKind, name: &str, value: &str) -> String {
    match tool {
        ToolKind::Curl | ToolKind::Wget => format!("{name}: {value}"),
        ToolKind::Httpie => format!("{name}:{value}"),
    }
}

/// Best-effort URL extraction across the three CLI tools we wrap.
///
/// `curl`/`wget` require a fully-qualified URL on the command line —
/// `find_url_in_args` already covers that. `httpie` accepts shorthands
/// like `:1402/path` (host defaults to `localhost`) or `example.com/path`
/// (scheme defaults to `http://`). We canonicalise both forms so the
/// SIWMPP cache lookup (URL-prefix match against tracked subscriptions)
/// sees the same URL the request will actually hit.
fn resolve_request_url(args: &[String], tool: ToolKind) -> Option<String> {
    if let Some(spec) = transfer_options(tool) {
        // The transfer target, not the first URL-looking argument: `-e
        // https://referer.example` is an option value.
        let line = CommandLine::parse(args, spec);
        let [target] = line.targets.as_slice() else {
            return None;
        };
        return [target.value]
            .into_iter()
            .find(|url| url.starts_with("http://") || url.starts_with("https://"))
            .map(str::to_string);
    }
    if let Some(u) = find_url_in_args(args) {
        return Some(u);
    }
    args.iter().find_map(|a| httpie_url_shorthand(a))
}

/// HTTPie-specific URL shorthand expander.
///
/// Skips flags, body items (`field=value`, `field:=jsonvalue`,
/// `field==query`), headers (`Header:value`), and HTTP method tokens.
/// Returns `Some(canonical_url)` for `:port[/path]` and bare
/// `host[:port][/path]` forms.
fn httpie_url_shorthand(arg: &str) -> Option<String> {
    if arg.starts_with('-') || arg.is_empty() {
        return None;
    }
    if arg.contains('=') {
        return None; // body item: key=val, key:=jsonval, key==query
    }
    if matches!(
        arg,
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "PATCH" | "OPTIONS"
    ) {
        return None;
    }

    // `:port[/path]` — host defaults to localhost.
    if let Some(rest) = arg.strip_prefix(':') {
        let first = rest.chars().next()?;
        if first.is_ascii_digit() {
            return Some(format!("http://localhost:{rest}"));
        }
        return None;
    }

    // Header item like `Foo:Bar` — colon present, but the part before is
    // a header name (letters/dashes), not a hostname.
    if let Some(colon) = arg.find(':') {
        let host_or_name = &arg[..colon];
        let after = arg[colon + 1..].chars().next();
        let port_like = after.is_some_and(|c| c.is_ascii_digit());
        let looks_hostlike = host_or_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        if !port_like || !looks_hostlike {
            return None;
        }
        // `host:port[/path]` → assume http://
        return Some(format!("http://{arg}"));
    }

    // Bare `host[/path]` — only treat as URL when it looks domain-ish
    // (contains a `.`), to avoid matching positional non-URL args.
    if arg.contains('.') {
        return Some(format!("http://{arg}"));
    }
    None
}

/// Pre-attach a cached SIWMPP `Authorization: Payment …` header to the
/// CLI command when a tracked subscription covers the request URL.
///
/// Returns the formatted header arg(s) to inject into the subprocess,
/// in the tool's native shape:
/// - curl/wget: `"Authorization: <token>"`
/// - httpie: `"Authorization:<token>"` (no space — HTTPie request-item form)
///
/// Empty vec when no subscription of the selected account matches the URL,
/// so callers pass it through to the existing inner runner unchanged.
fn pre_attach_cached_auth(
    args: &[String],
    tool: ToolKind,
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> Vec<String> {
    let Some(url) = resolve_request_url(args, tool) else {
        return Vec::new();
    };
    // Skip pre-attach when the user already passed their own
    // Authorization header — their value wins.
    if user_already_set_auth(args, tool) {
        return Vec::new();
    }
    let store = crate::accounts::FileAccountsStore::default_path();
    let Some(token) = crate::client::authenticate::cached_header_for_account(
        &store,
        network_override,
        account_override,
        &url,
    ) else {
        return Vec::new();
    };
    vec![formatted_header(tool, "Authorization", &token)]
}

/// True when the user already passed an Authorization header through
/// the wrapper's args, so we don't clobber it with a cached token.
fn user_already_set_auth(args: &[String], tool: ToolKind) -> bool {
    user_already_set_header(args, tool, "authorization")
}

fn user_already_set_user_agent(args: &[String], tool: ToolKind) -> bool {
    if user_already_set_header(args, tool, "user-agent") {
        return true;
    }
    if let ToolKind::Wget = tool {
        return wget_options(args).any(|(name, value)| {
            matches!(
                name,
                OptionName::Short('U') | OptionName::Long("user-agent")
            ) && value.is_some()
        });
    }
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        match tool {
            ToolKind::Curl => {
                if arg == "-A" || arg == "--user-agent" {
                    return iter.peek().is_some();
                }
                if arg.starts_with("-A") && arg.len() > 2 {
                    return true;
                }
                if arg.starts_with("--user-agent=") {
                    return true;
                }
            }
            ToolKind::Wget | ToolKind::Httpie => {}
        }
    }
    false
}

fn user_already_set_header(args: &[String], tool: ToolKind, header_name: &str) -> bool {
    if let ToolKind::Wget = tool {
        return wget_options(args).any(|(name, value)| {
            name == OptionName::Long("header")
                && value.is_some_and(|value| is_header_name(value, header_name))
        });
    }
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        match tool {
            ToolKind::Curl => {
                if (arg == "-H" || arg == "--header")
                    && let Some(next) = iter.peek()
                    && is_header_name(next, header_name)
                {
                    return true;
                }
                if let Some(val) = arg
                    .strip_prefix("-H")
                    .or_else(|| arg.strip_prefix("--header="))
                    && is_header_name(val, header_name)
                {
                    return true;
                }
            }
            ToolKind::Wget => {}
            ToolKind::Httpie => {
                if is_header_name(arg, header_name) {
                    return true;
                }
            }
        }
    }
    false
}

fn is_header_name(header: &str, expected: &str) -> bool {
    header
        .split_once(':')
        .map(|(name, _)| name.trim().eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

#[cfg(test)]
mod pre_attach_tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn httpie_shorthand_with_port_resolves_to_localhost() {
        let args = s(&["-v", "POST", ":1402/", "jsonrpc=2.0", "id:=1"]);
        assert_eq!(
            resolve_request_url(&args, ToolKind::Httpie),
            Some("http://localhost:1402/".to_string())
        );
    }

    #[test]
    fn httpie_shorthand_with_host_port_resolves_with_default_scheme() {
        let args = s(&["GET", "example.com:8080/api"]);
        assert_eq!(
            resolve_request_url(&args, ToolKind::Httpie),
            Some("http://example.com:8080/api".to_string())
        );
    }

    #[test]
    fn httpie_skips_header_items_and_body_fields() {
        let args = s(&["POST", "Authorization:Bearer x", "field=value", ":1402/"]);
        assert_eq!(
            resolve_request_url(&args, ToolKind::Httpie),
            Some("http://localhost:1402/".to_string())
        );
    }

    #[test]
    fn user_auth_overrides_curl_pre_attach() {
        let args = s(&["-H", "Authorization: Bearer existing", "http://example.com"]);
        assert!(user_already_set_auth(&args, ToolKind::Curl));
    }

    #[test]
    fn user_auth_overrides_httpie_pre_attach() {
        let args = s(&["GET", ":1402/", "Authorization:Bearer existing"]);
        assert!(user_already_set_auth(&args, ToolKind::Httpie));
    }

    #[test]
    fn curl_injects_default_cli_user_agent() {
        let args = s(&["https://example.com"]);
        let headers = headers_with_default_user_agent(&args, &[], ToolKind::Curl);
        assert_eq!(
            headers,
            vec![format!("User-Agent: {}", ClientApp::Cli.user_agent())]
        );
    }

    #[test]
    fn httpie_injects_default_cli_user_agent_in_request_item_form() {
        let args = s(&["GET", "example.com"]);
        let headers = headers_with_default_user_agent(&args, &[], ToolKind::Httpie);
        assert_eq!(
            headers,
            vec![format!("User-Agent:{}", ClientApp::Cli.user_agent())]
        );
    }

    #[test]
    fn user_agent_header_overrides_default_for_wget() {
        let args = s(&[
            "--header",
            "User-Agent: custom-client",
            "https://example.com",
        ]);
        let headers = headers_with_default_user_agent(&args, &[], ToolKind::Wget);
        assert!(headers.is_empty());
    }

    #[test]
    fn curl_user_agent_flag_overrides_default() {
        let args = s(&["-A", "custom-client", "https://example.com"]);
        let headers = headers_with_default_user_agent(&args, &[], ToolKind::Curl);
        assert!(headers.is_empty());
    }

    #[test]
    fn extra_user_agent_header_overrides_default() {
        let args = s(&["https://example.com"]);
        let extra = s(&["User-Agent: retry-client"]);
        let headers = headers_with_default_user_agent(&args, &extra, ToolKind::Curl);
        assert_eq!(headers, extra);
    }

    /// Loopback origin that answers its first request with a 302 to a
    /// second, collector origin, and reports what each one received once
    /// the wrapped tool has exited.
    struct CrossOriginRedirect {
        origin_url: String,
        collector_url: String,
        origin: std::thread::JoinHandle<Vec<String>>,
        collector: std::thread::JoinHandle<Vec<String>>,
    }

    /// Serve one connection per entry of `responses`, in order, or give up
    /// after `wait`. Returns the raw head of every request that arrived.
    fn serve(
        listener: std::net::TcpListener,
        wait: std::time::Duration,
        responses: Vec<String>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        use std::io::{Read, Write};
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + wait;
            let mut requests = Vec::new();
            for response in responses {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(_) if std::time::Instant::now() < deadline => {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(_) => return requests,
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ = stream.write_all(response.as_bytes());
                requests.push(String::from_utf8_lossy(&request).into_owned());
            }
            requests
        })
    }

    fn redirect_to(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    const OK_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
    const PAYMENT_REQUIRED_RESPONSE: &str =
        "HTTP/1.1 402 Payment Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    impl CrossOriginRedirect {
        fn start() -> Self {
            let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let collector = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let origin_url = format!("http://{}/start", origin.local_addr().unwrap());
            let location = format!("http://{}/collect", collector.local_addr().unwrap());
            let origin = serve(
                origin,
                std::time::Duration::from_secs(20),
                vec![redirect_to(&location)],
            );
            let collector = serve(
                collector,
                std::time::Duration::from_secs(3),
                vec![OK_RESPONSE.to_string()],
            );
            Self {
                origin_url,
                collector_url: location,
                origin,
                collector,
            }
        }

        /// What the origin and the collector each received.
        fn requests(self) -> (String, Option<String>) {
            let origin = self.origin.join().unwrap().pop();
            let collector = self.collector.join().unwrap().pop();
            (
                origin.expect("the tool never reached the origin"),
                collector,
            )
        }

        fn collector_was_contacted(self) -> bool {
            self.requests().1.is_some()
        }
    }

    const PAYMENT_HEADER: &str = "Authorization: Payment redirect-test-credential";
    const PAYMENT_SECRET: &str = "redirect-test-credential";

    #[test]
    fn redirect_guard_only_applies_when_credentials_are_injected() {
        assert!(credential_redirect_guard(ToolKind::Wget, &[]).is_empty());
        let injected = s(&[PAYMENT_HEADER]);
        assert_eq!(
            credential_redirect_guard(ToolKind::Curl, &injected),
            ["--no-location", "--globoff"]
        );
        assert_eq!(
            credential_redirect_guard(ToolKind::Wget, &injected),
            ["--max-redirect=0"]
        );
        assert_eq!(
            credential_redirect_guard(ToolKind::Httpie, &injected),
            ["--no-follow"]
        );
    }

    #[test]
    fn wget_follows_cross_origin_redirects_without_injected_credentials() {
        // Control for the guard tests below: wget follows redirects by
        // default, so the collector is reachable when nothing is injected.
        if check_command_exists("wget").is_err() {
            return;
        }
        let server = CrossOriginRedirect::start();
        let args = s(&["-q", "-O", "-", &server.origin_url]);
        run_wget_inner(&args, &[]).unwrap();
        assert!(server.collector_was_contacted());
    }

    #[test]
    fn wget_follows_a_paid_redirect_without_the_credential() {
        if check_command_exists("wget").is_err() {
            return;
        }
        let server = CrossOriginRedirect::start();
        let args = s(&["-q", "--max-redirect=5", "-O", "-", &server.origin_url]);
        let outcome = run_wget_inner(&args, &s(&[PAYMENT_HEADER])).unwrap();
        assert!(matches!(
            outcome,
            RunOutcome::Completed { exit_code: 0, .. }
        ));
        let (origin, collector) = server.requests();
        assert!(
            origin.contains(PAYMENT_SECRET),
            "the paid URL got the credential"
        );
        let collector = collector.expect("wget stopped at the paid redirect");
        assert!(!collector.contains(PAYMENT_SECRET), "{collector}");
    }

    #[test]
    fn wget_does_not_follow_a_paid_redirect_the_caller_turned_off() {
        if check_command_exists("wget").is_err() {
            return;
        }
        let server = CrossOriginRedirect::start();
        let args = s(&["-q", "--max-redirect", "0", "-O", "-", &server.origin_url]);
        run_wget_inner(&args, &s(&[PAYMENT_HEADER])).unwrap();
        assert!(!server.collector_was_contacted());
    }

    #[test]
    fn curl_follows_a_paid_redirect_without_the_credential() {
        if check_command_exists("curl").is_err() {
            return;
        }
        let server = CrossOriginRedirect::start();
        // Even `--location-trusted`, which forwards credentials by design,
        // must not hand Pay's header to the next hop.
        let args = s(&["-s", "--location-trusted", &server.origin_url]);
        let outcome = run_curl_inner(&args, &s(&[PAYMENT_HEADER])).unwrap();
        assert!(matches!(
            outcome,
            RunOutcome::Completed { exit_code: 0, .. }
        ));
        let (origin, collector) = server.requests();
        assert!(
            origin.contains(PAYMENT_SECRET),
            "the paid URL got the credential"
        );
        let collector = collector.expect("curl stopped at the paid redirect");
        assert!(!collector.contains(PAYMENT_SECRET), "{collector}");
    }

    #[test]
    fn curl_does_not_follow_a_paid_redirect_unless_asked() {
        if check_command_exists("curl").is_err() {
            return;
        }
        for flags in [
            vec!["-s"],
            vec!["-sL", "--no-location"],
            vec!["-sL", "-d", "a=1"],
        ] {
            let server = CrossOriginRedirect::start();
            let mut args = s(&flags);
            args.push(server.origin_url.clone());
            run_curl_inner(&args, &s(&[PAYMENT_HEADER])).unwrap();
            assert!(!server.collector_was_contacted(), "{flags:?}");
        }
    }

    /// `/a` redirects to `/a/` (a trailing-slash canonicalisation, the same
    /// shape as an `http` to `https` upgrade), and `/a/` is the paid
    /// resource: it asks for payment, then serves the paid retry.
    fn trailing_slash_paywall() -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/a", listener.local_addr().unwrap());
        let server = serve(
            listener,
            std::time::Duration::from_secs(20),
            vec![
                redirect_to("/a/"),
                PAYMENT_REQUIRED_RESPONSE.to_string(),
                OK_RESPONSE.to_string(),
            ],
        );
        (url, server)
    }

    fn request_path(request: &str) -> &str {
        request.split_whitespace().nth(1).unwrap_or_default()
    }

    fn pay_behind_a_redirect(tool: ToolKind, args: Vec<String>, url: &str) {
        let first = match tool {
            ToolKind::Curl => run_curl_inner(&args, &[]),
            ToolKind::Wget => run_wget_inner(&args, &[]),
            ToolKind::Httpie => unreachable!(),
        }
        .unwrap();
        let RunOutcome::UnknownPaymentRequired { resource_url, .. } = first else {
            panic!("expected the 402 from {url}/: {first:?}");
        };
        assert_eq!(resource_url, format!("{url}/"));
        let paid = run_paid(tool, &args, &resource_url, &s(&[PAYMENT_HEADER])).unwrap();
        assert!(
            matches!(paid, RunOutcome::Completed { exit_code: 0, .. }),
            "{paid:?}"
        );
    }

    #[test]
    fn curl_pays_the_url_that_asked_for_payment_behind_a_redirect() {
        if check_command_exists("curl").is_err() {
            return;
        }
        let (url, server) = trailing_slash_paywall();
        pay_behind_a_redirect(ToolKind::Curl, s(&["-sL", &url]), &url);
        let requests = server.join().unwrap();
        let seen: Vec<(&str, bool)> = requests
            .iter()
            .map(|r| (request_path(r), r.contains(PAYMENT_SECRET)))
            .collect();
        assert_eq!(seen, [("/a", false), ("/a/", false), ("/a/", true)]);
    }

    #[test]
    fn wget_pays_the_url_that_asked_for_payment_behind_a_redirect() {
        if check_command_exists("wget").is_err() {
            return;
        }
        let (url, server) = trailing_slash_paywall();
        pay_behind_a_redirect(ToolKind::Wget, s(&["-q", "-O", "-", &url]), &url);
        let requests = server.join().unwrap();
        let seen: Vec<(&str, bool)> = requests
            .iter()
            .map(|r| (request_path(r), r.contains(PAYMENT_SECRET)))
            .collect();
        assert_eq!(seen, [("/a", false), ("/a/", false), ("/a/", true)]);
    }

    #[test]
    fn a_402_from_another_origin_is_not_paid() {
        if check_command_exists("curl").is_err() {
            return;
        }
        let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_url = format!("http://{}/start", origin.local_addr().unwrap());
        let other_url = format!("http://{}/paid", other.local_addr().unwrap());
        let wait = std::time::Duration::from_secs(20);
        let origin = serve(origin, wait, vec![redirect_to(&other_url)]);
        let other = serve(other, wait, vec![PAYMENT_REQUIRED_RESPONSE.to_string()]);
        let outcome = run_curl_inner(&s(&["-sL", &origin_url]), &[]);
        assert!(
            matches!(&outcome, Err(Error::RequestValidation(m)) if m.contains(&other_url)),
            "{outcome:?}"
        );
        assert_eq!(origin.join().unwrap().len(), 1);
        assert_eq!(other.join().unwrap().len(), 1);
    }

    #[test]
    fn the_paying_url_follows_the_redirect_chain_within_one_origin() {
        let head = |status, location: Option<&str>| ResponseHead {
            status: Some(status),
            headers: location
                .map(|l| vec![("location".to_string(), l.to_string())])
                .unwrap_or_default(),
        };
        let get = s(&["-sL", "http://a.example/x"]);
        let chain = [
            head(301, Some("https://a.example/x")),
            head(302, Some("/x/")),
            head(402, None),
        ];
        assert_eq!(
            payment_url(&get, ToolKind::Curl, &chain, "http://a.example/x").unwrap(),
            "https://a.example/x/"
        );
        // No redirect: the URL as the caller wrote it.
        assert_eq!(
            payment_url(
                &get,
                ToolKind::Curl,
                &[head(402, None)],
                "http://a.example/x"
            )
            .unwrap(),
            "http://a.example/x"
        );
        // A redirect the tool did not follow (the last head) does not move it.
        assert_eq!(
            answering_url("http://a.example/x", &[head(302, Some("/y"))]),
            "http://a.example/x"
        );
        for elsewhere in [
            "https://b.example/x",
            "https://a.example:8443/x",
            "http://b.a.example/",
        ] {
            let chain = [head(302, Some(elsewhere)), head(402, None)];
            assert!(
                payment_url(&get, ToolKind::Curl, &chain, "http://a.example/x").is_err(),
                "{elsewhere}"
            );
        }
    }

    #[test]
    fn the_paid_retry_replays_only_the_request_that_got_the_402() {
        let head = |status, location: Option<&str>| ResponseHead {
            status: Some(status),
            headers: location
                .map(|l| vec![("location".to_string(), l.to_string())])
                .unwrap_or_default(),
        };
        let via = |status| [head(status, Some("/paid")), head(402, None)];
        let url = "https://a.example/start";
        let pays = |args: &[&str], tool, chain: &[ResponseHead]| {
            let mut args = s(args);
            args.push(url.to_string());
            payment_url(&args, tool, chain, url)
        };
        // A body sent again only on 307 and 308; 301, 302 and 303 turned the
        // POST into a bodyless GET.
        for status in [301, 302, 303] {
            assert!(pays(&["-L", "-d", "x=1"], ToolKind::Curl, &via(status)).is_err());
            assert!(pays(&["--post-data=x=1"], ToolKind::Wget, &via(status)).is_err());
            // `-X` and `--post30x` change what the tool sent: refused too.
            assert!(
                pays(
                    &["-L", "-X", "POST", "-d", "x"],
                    ToolKind::Curl,
                    &via(status)
                )
                .is_err()
            );
            assert!(
                pays(
                    &["-L", "--post302", "-d", "x"],
                    ToolKind::Curl,
                    &via(status)
                )
                .is_err()
            );
            assert!(pays(&["-L", "-X", "DELETE"], ToolKind::Curl, &via(status)).is_err());
        }
        for status in [307, 308] {
            assert_eq!(
                pays(&["-L", "-d", "x=1"], ToolKind::Curl, &via(status)).unwrap(),
                "https://a.example/paid"
            );
            assert!(pays(&["--post-data=x=1"], ToolKind::Wget, &via(status)).is_ok());
            // A body read from stdin cannot be sent a second time.
            assert!(pays(&["-L", "-d", "@-"], ToolKind::Curl, &via(status)).is_err());
            assert!(pays(&["-L", "-T", "-"], ToolKind::Curl, &via(status)).is_err());
            assert!(pays(&["--post-file=-"], ToolKind::Wget, &via(status)).is_err());
            for stdin in [
                &["--data-binary", "@-"][..],
                &["--json", "@-"],
                &["--data-urlencode", "@-"],
                &["--data-urlencode", "q@-"],
                &["-F", "f=@-"],
                &["-F", "f=<-;type=text/plain"],
            ] {
                let args = [&["-L"][..], stdin].concat();
                assert!(
                    pays(&args, ToolKind::Curl, &via(status)).is_err(),
                    "{stdin:?}"
                );
            }
            // Bodies that only look like stdin are literal and can be resent.
            for literal in [
                &["--data-raw", "@-"][..],
                &["--data-raw", "a<-b"],
                &["--form-string", "f=@-"],
                &["--form-string", "f=<-"],
                &["-d", "a@-b"],
                &["--data-urlencode", "q=@-"],
                &["-F", "f=a<-b"],
            ] {
                let args = [&["-L"][..], literal].concat();
                assert!(
                    pays(&args, ToolKind::Curl, &via(status)).is_ok(),
                    "{literal:?}"
                );
            }
        }
        // Positive controls: a GET or HEAD is the same request after any
        // redirect, and without a redirect any request is replayed as is.
        for status in [301, 302, 303, 307, 308] {
            assert!(pays(&["-L"], ToolKind::Curl, &via(status)).is_ok());
            assert!(pays(&["-LI"], ToolKind::Curl, &via(status)).is_ok());
            assert!(pays(&["-q"], ToolKind::Wget, &via(status)).is_ok());
        }
        for args in [&["-d", "@-"][..], &["-X", "DELETE"], &["-T", "-"]] {
            assert_eq!(pays(args, ToolKind::Curl, &[head(402, None)]).unwrap(), url);
        }
    }

    #[test]
    fn curl_refuses_to_pay_for_a_post_a_redirect_turned_into_a_get() {
        if check_command_exists("curl").is_err() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let start = format!("http://{}/start", listener.local_addr().unwrap());
        let server = serve(
            listener,
            std::time::Duration::from_secs(20),
            vec![
                "HTTP/1.1 303 See Other\r\nLocation: /paid\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                PAYMENT_REQUIRED_RESPONSE.to_string(),
            ],
        );
        let outcome = run_curl_inner(&s(&["-sL", "-d", "x=1", &start]), &[]);
        assert!(
            matches!(&outcome, Err(Error::RequestValidation(m)) if m.contains("/paid")),
            "{outcome:?}"
        );
        let methods: Vec<String> = server
            .join()
            .unwrap()
            .iter()
            .map(|r| r.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(methods, ["POST /start", "GET /paid"]);
    }

    #[test]
    fn curl_pays_a_post_that_a_307_kept_a_post() {
        if check_command_exists("curl").is_err() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let start = format!("http://{}/start", listener.local_addr().unwrap());
        let server = serve(
            listener,
            std::time::Duration::from_secs(20),
            vec![
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: /paid\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                PAYMENT_REQUIRED_RESPONSE.to_string(),
                OK_RESPONSE.to_string(),
            ],
        );
        let args = s(&["-sL", "-d", "x=1", &start]);
        let RunOutcome::UnknownPaymentRequired { resource_url, .. } =
            run_curl_inner(&args, &[]).unwrap()
        else {
            panic!("expected the 402 from /paid");
        };
        let paid = run_paid(ToolKind::Curl, &args, &resource_url, &s(&[PAYMENT_HEADER])).unwrap();
        assert!(matches!(paid, RunOutcome::Completed { exit_code: 0, .. }));
        let requests = server.join().unwrap();
        let seen: Vec<(&str, &str, bool)> = requests
            .iter()
            .map(|r| {
                let mut line = r.split_whitespace();
                (
                    line.next().unwrap_or_default(),
                    line.next().unwrap_or_default(),
                    r.contains(PAYMENT_SECRET),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("POST", "/start", false),
                ("POST", "/paid", false),
                ("POST", "/paid", true)
            ]
        );
    }

    #[test]
    fn redirect_following_reads_the_callers_flags() {
        assert!(follows_redirects(&s(&["-sL", "u"]), ToolKind::Curl));
        assert!(follows_redirects(
            &s(&["--location-trusted", "u"]),
            ToolKind::Curl
        ));
        assert!(!follows_redirects(&s(&["-s", "u"]), ToolKind::Curl));
        assert!(!follows_redirects(
            &s(&["-L", "--no-location", "u"]),
            ToolKind::Curl
        ));
        // `-e L` is a referer value, not `-L`.
        assert!(!follows_redirects(&s(&["-eL", "u"]), ToolKind::Curl));
        assert!(follows_redirects(&s(&["-q", "u"]), ToolKind::Wget));
        assert!(follows_redirects(
            &s(&["--max-redirect=3", "u"]),
            ToolKind::Wget
        ));
        assert!(!follows_redirects(
            &s(&["--max-redirect=0", "u"]),
            ToolKind::Wget
        ));
        assert!(!follows_redirects(
            &s(&["--max-redirect", "0", "u"]),
            ToolKind::Wget
        ));
        assert!(follows_redirects(&s(&["--follow", "u"]), ToolKind::Httpie));
        assert!(!follows_redirects(&s(&["u"]), ToolKind::Httpie));
    }

    #[test]
    fn only_bodyless_reads_are_replayed_at_a_redirect_target() {
        assert!(is_bodyless_read(&s(&["-sL", "u"]), ToolKind::Curl));
        assert!(is_bodyless_read(&s(&["-I", "u"]), ToolKind::Curl));
        assert!(is_bodyless_read(&s(&["-X", "GET", "u"]), ToolKind::Curl));
        for args in [
            s(&["-d", "a=1", "u"]),
            s(&["-sd", "a=1", "u"]),
            s(&["--json", "{}", "u"]),
            s(&["-F", "f=@x", "u"]),
            s(&["-T", "file", "u"]),
            s(&["-X", "DELETE", "u"]),
        ] {
            assert!(!is_bodyless_read(&args, ToolKind::Curl), "{args:?}");
        }
        assert!(is_bodyless_read(&s(&["-q", "u"]), ToolKind::Wget));
        assert!(!is_bodyless_read(
            &s(&["--post-data=a", "u"]),
            ToolKind::Wget
        ));
        assert!(!is_bodyless_read(
            &s(&["--method=PUT", "u"]),
            ToolKind::Wget
        ));
        assert!(is_bodyless_read(
            &s(&["GET", "u", "Accept:x", "q==1"]),
            ToolKind::Httpie
        ));
        assert!(!is_bodyless_read(&s(&["u", "a=1"]), ToolKind::Httpie));
        assert!(!is_bodyless_read(&s(&["POST", "u"]), ToolKind::Httpie));
    }

    #[test]
    fn the_paid_request_is_retargeted_in_place() {
        let to = "https://a.example/x/";
        assert_eq!(
            paid_request_args(
                &s(&["-sL", "-e", "https://ref.example", "http://a.example/x"]),
                ToolKind::Curl,
                to
            ),
            s(&["-sL", "-e", "https://ref.example", to])
        );
        assert_eq!(
            paid_request_args(&s(&["--url=http://a.example/x"]), ToolKind::Curl, to),
            s(&[&format!("--url={to}")])
        );
        assert_eq!(
            paid_request_args(&s(&["-q", "http://a.example/x"]), ToolKind::Wget, to),
            s(&["-q", to])
        );
        assert_eq!(
            paid_request_args(
                &s(&["--follow", "http://a.example/x"]),
                ToolKind::Httpie,
                to
            ),
            s(&["--follow", to])
        );
        // Unchanged when it already targets the URL.
        assert_eq!(
            paid_request_args(&s(&["-q", to]), ToolKind::Wget, to),
            s(&["-q", to])
        );
    }

    #[test]
    fn a_single_transfer_is_accepted() {
        for args in [
            s(&[
                "-sS",
                "-H",
                "Accept: x",
                "-d",
                "a=1",
                "https://a.example/paid",
            ]),
            s(&["-o", "out.json", "--url", "https://a.example/paid"]),
            s(&[
                "-X",
                "POST",
                "-ehttps://referer.example",
                "https://a.example/paid",
            ]),
            s(&[
                "--max-time",
                "5",
                "-w",
                "%{http_code}",
                "https://a.example/paid",
            ]),
        ] {
            assert!(
                ensure_single_transfer(&args, ToolKind::Curl).is_ok(),
                "{args:?}"
            );
        }
        for args in [
            s(&["-q", "-O", "-", "https://a.example/paid"]),
            s(&[
                "-nv",
                "--header",
                "Accept: x",
                "-Oout",
                "https://a.example/paid",
            ]),
            s(&["--post-data=a=1", "-U", "agent", "https://a.example/paid"]),
        ] {
            assert!(
                ensure_single_transfer(&args, ToolKind::Wget).is_ok(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn curl_invocations_with_more_than_one_transfer_are_refused() {
        for args in [
            s(&["https://a.example/paid", "https://collector.example/"]),
            s(&["https://a.example/paid", "collector.example"]),
            s(&[
                "--url",
                "https://a.example/paid",
                "https://collector.example/",
            ]),
            s(&[
                "https://a.example/paid",
                "--next",
                "https://collector.example/",
            ]),
            s(&["https://a.example/paid", "-:", "https://collector.example/"]),
            s(&["-K", "urls.txt", "https://a.example/paid"]),
            s(&["-sK", "urls.txt"]),
            s(&["--url", "@urls.txt"]),
            s(&["--variable", "u=x", "--expand-url", "{{u}}"]),
            s(&[
                "-d",
                "a=1",
                "--",
                "https://a.example/paid",
                "https://c.example/",
            ]),
        ] {
            assert!(
                ensure_single_transfer(&args, ToolKind::Curl).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn wget_invocations_with_more_than_one_transfer_are_refused() {
        for args in [
            s(&["https://a.example/paid", "https://collector.example/"]),
            s(&["-i", "urls.txt"]),
            s(&["--input-file=urls.txt"]),
            s(&["-r", "https://a.example/paid"]),
            s(&["-qm", "https://a.example/paid"]),
            s(&["--page-requisites", "https://a.example/paid"]),
            s(&["-e", "recursive=on", "https://a.example/paid"]),
        ] {
            assert!(
                ensure_single_transfer(&args, ToolKind::Wget).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn wget_abbreviations_are_read_as_the_option_they_select() {
        // getopt_long accepts any unambiguous prefix of a long option.
        for args in [
            s(&["--input-fil=urls.txt"]),
            s(&["--input-fil", "urls.txt"]),
            s(&["--mirro", "https://a.example/paid"]),
            s(&["--recur", "https://a.example/paid"]),
            s(&["--page-req", "https://a.example/paid"]),
            s(&["--exec", "recursive=on", "https://a.example/paid"]),
            s(&["--conf=evil.wgetrc", "https://a.example/paid"]),
            s(&["--metalink-over", "https://a.example/paid"]),
            // Ambiguous, and one reading (`--config`) adds transfers.
            s(&["--con", "x", "https://a.example/paid"]),
            // Booleans take no separate value: both URLs are transfers.
            s(&[
                "--backups",
                "https://collector.example/",
                "https://a.example/paid",
            ]),
        ] {
            assert!(
                ensure_single_transfer(&args, ToolKind::Wget).is_err(),
                "{args:?}"
            );
        }
        for args in [
            s(&["--head", "Accept: x", "https://a.example/paid"]),
            s(&["--output-doc=-", "https://a.example/paid"]),
            s(&["--no-verb", "--quiet", "https://a.example/paid"]),
            s(&["--report-speed=bits", "https://a.example/paid"]),
            s(&["--no-recursive", "https://a.example/paid"]),
        ] {
            assert!(
                ensure_single_transfer(&args, ToolKind::Wget).is_ok(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn wget_abbreviations_reach_every_reading_of_the_command_line() {
        assert!(!follows_redirects(
            &s(&["--max-redir=0", "u"]),
            ToolKind::Wget
        ));
        assert!(!is_bodyless_read(
            &s(&["--post-d=a=1", "u"]),
            ToolKind::Wget
        ));
        assert!(user_already_set_auth(
            &s(&["--head=Authorization: Bearer mine", "https://a.example/"]),
            ToolKind::Wget
        ));
        assert!(user_already_set_user_agent(
            &s(&["--user-a", "agent", "https://a.example/"]),
            ToolKind::Wget
        ));
        let request =
            ParsedWgetRequest::from_args(&s(&["--post-d", "a=1", "https://a.example/paid"]));
        assert_eq!(request.method, "POST");
        assert_eq!(request.body.as_deref(), Some("a=1"));
        assert_eq!(request.url.as_deref(), Some("https://a.example/paid"));
    }

    #[test]
    fn curl_long_options_are_never_abbreviated() {
        // curl rejects `--nex` as unknown rather than reading `--next`, so
        // it is a flag here too; the URL count still holds the line.
        assert!(
            ensure_single_transfer(&s(&["--nex", "https://a.example/paid"]), ToolKind::Curl)
                .is_ok()
        );
        assert!(
            ensure_single_transfer(
                &s(&["https://a.example/paid", "--nex", "https://c.example/"]),
                ToolKind::Curl
            )
            .is_err()
        );
    }

    #[test]
    fn credentials_are_never_injected_into_a_multi_transfer_command() {
        // The public retry entry points refuse before spawning anything.
        let args = s(&["https://a.example/paid", "https://collector.example/"]);
        let payment = s(&[PAYMENT_HEADER]);
        if check_command_exists("curl").is_ok() {
            assert!(matches!(
                run_curl_inner(&args, &payment),
                Err(Error::RequestValidation(_))
            ));
        }
        if check_command_exists("wget").is_ok() {
            assert!(matches!(
                run_wget_inner(&args, &payment),
                Err(Error::RequestValidation(_))
            ));
        }
        // And so does the pre-signing validation the CLI runs.
        assert!(validate_curl_args_against_catalog(&args).is_err());
        assert!(validate_wget_args_against_catalog(&args).is_err());
    }

    #[test]
    fn curl_url_globs_do_not_fan_credentials_out() {
        if check_command_exists("curl").is_err() {
            return;
        }
        let server = CrossOriginRedirect::start();
        // Without --globoff this glob would be two transfers: the origin and
        // the collector. With it, curl requests a literal (invalid) URL.
        let collector = server.collector_url.clone();
        let glob = format!("{{{},{}}}", server.origin_url, collector);
        let _ = run_curl_inner(&s(&["-s", &glob]), &s(&[PAYMENT_HEADER]));
        let _ = run_curl_inner(&s(&["-s", &server.origin_url]), &[]);
        assert!(!server.collector_was_contacted());
    }

    /// Collects every formatted field of every event it sees.
    #[derive(Clone, Default)]
    struct EventText(std::sync::Arc<std::sync::Mutex<String>>);

    impl tracing::Subscriber for EventText {
        fn register_callsite(
            &self,
            _: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::sometimes()
        }
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor<'a>(&'a mut String);
            impl tracing::field::Visit for Visitor<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write;
                    let _ = write!(self.0, "{}={value:?};", field.name());
                }
            }
            event.record(&mut Visitor(&mut self.0.lock().unwrap()));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn invocation_logs_carry_header_names_but_no_secrets() {
        if check_command_exists("curl").is_err() {
            return;
        }
        // A process-wide subscriber: a scoped one races other tests over
        // the callsite interest cache and can miss the event.
        static EVENTS: std::sync::OnceLock<EventText> = std::sync::OnceLock::new();
        let events = EVENTS.get_or_init(|| {
            let events = EventText::default();
            let _ = tracing::subscriber::set_global_default(events.clone());
            events
        });
        let server = CrossOriginRedirect::start();
        let args = s(&[
            "-s",
            "-H",
            "X-Api-Key: caller-secret",
            "-u",
            "user:caller-password",
            &server.origin_url,
        ]);
        run_curl_inner(&args, &s(&[PAYMENT_HEADER])).unwrap();
        let _ = server.collector_was_contacted();
        let logged = events.0.lock().unwrap().clone();
        // Positive control: the event was captured and names the headers.
        assert!(logged.contains("Authorization"), "{logged}");
        assert!(logged.contains("User-Agent"), "{logged}");
        for secret in [
            "redirect-test-credential",
            "caller-secret",
            "caller-password",
        ] {
            assert!(
                !logged.contains(secret),
                "a caller credential leaked into the logged event"
            );
        }
    }

    #[test]
    fn no_url_means_no_pre_attach() {
        let args = s(&["--help"]);
        assert_eq!(resolve_request_url(&args, ToolKind::Httpie), None);
        assert!(pre_attach_cached_auth(&args, ToolKind::Httpie, None, None).is_empty());
    }
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
        content_type: None,
        response_headers: Vec::new(),
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
        let url = resolve_request_url(args, ToolKind::Wget);
        let mut explicit_method = None;
        let mut body = None;
        let mut post_body_seen = false;
        for (name, value) in wget_options(args) {
            match name {
                OptionName::Long("method") => {
                    if let Some(value) = value {
                        explicit_method = Some(value.to_ascii_uppercase());
                    }
                }
                OptionName::Long("post-data" | "body-data") => {
                    if let Some(value) = value {
                        body = Some(value.to_string());
                    }
                    post_body_seen = true;
                }
                OptionName::Long("post-file" | "body-file") => post_body_seen = true,
                OptionName::Long("spider") => {
                    explicit_method.get_or_insert_with(|| "HEAD".to_string());
                }
                _ => {}
            }
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
        assert_eq!(
            outcome.payment_network(None).unwrap().as_deref(),
            Some("devnet")
        );
        assert_eq!(
            outcome
                .payment_network(Some("localnet"))
                .unwrap()
                .as_deref(),
            Some("localnet")
        );
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
                let first: pay_kit::mpp::ChargeRequest = challenge.request.decode().unwrap();
                assert_eq!(first.currency, "USDC");
                assert_eq!(alternatives.len(), 2);
            }
            other => panic!("expected MppChallenge, got {other:?}"),
        }
    }

    #[test]
    fn classify_402_with_subscription_intent() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let request_json = serde_json::json!({
            "amount": "10000000",
            "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "periodUnit": "day",
            "periodCount": "30",
            "recipient": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
            "methodDetails": {
                "planAddress": "8tWbqLkUJoYy7zXc5h2EvCRoaQEv2xnQjUuYhc3rzCgT",
                "subscriptionProgram": pay_kit::mpp::program::subscriptions::SUBSCRIPTIONS_PROGRAM_ID,
                "mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "puller": "5fKb5cF22cFybZB1H4hLDydFhwoQy9JzKzRWaSbMkB6h",
                "decimals": 6,
                "network": "mainnet"
            }
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request_json).unwrap());
        let headers = vec![(
            "www-authenticate".to_string(),
            format!(
                "Payment id=\"sub-1\", realm=\"test\", method=\"solana\", \
                 intent=\"subscription\", request=\"{b64}\""
            ),
        )];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(
            matches!(outcome, RunOutcome::SubscriptionChallenge { .. }),
            "expected SubscriptionChallenge, got {outcome:?}"
        );
    }

    #[test]
    fn classify_402_prefers_subscription_over_charge_when_both_present() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        // Charge challenge first.
        let charge = serde_json::json!({
            "amount": "1000000",
            "currency": "USDC",
            "recipient": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
            "methodDetails": {"network": "mainnet"}
        });
        let charge_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&charge).unwrap());
        let charge_header = format!(
            "Payment id=\"c\", realm=\"r\", method=\"solana\", intent=\"charge\", \
             request=\"{charge_b64}\""
        );

        // Subscription challenge second.
        let sub = serde_json::json!({
            "amount": "10000000",
            "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "periodUnit": "day",
            "periodCount": "30",
            "recipient": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
            "methodDetails": {
                "planAddress": "8tWbqLkUJoYy7zXc5h2EvCRoaQEv2xnQjUuYhc3rzCgT",
                "subscriptionProgram": pay_kit::mpp::program::subscriptions::SUBSCRIPTIONS_PROGRAM_ID,
                "mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "puller": "5fKb5cF22cFybZB1H4hLDydFhwoQy9JzKzRWaSbMkB6h",
                "decimals": 6,
                "network": "mainnet"
            }
        });
        let sub_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&sub).unwrap());
        let sub_header = format!(
            "Payment id=\"s\", realm=\"r\", method=\"solana\", intent=\"subscription\", \
             request=\"{sub_b64}\""
        );

        let headers = vec![
            ("www-authenticate".to_string(), charge_header),
            ("www-authenticate".to_string(), sub_header),
        ];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(
            matches!(outcome, RunOutcome::SubscriptionChallenge { .. }),
            "subscription must win over charge: {outcome:?}"
        );
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

    /// A `batch-settlement` offer carries the same core fields as `exact`
    /// (scheme, network, amount, asset, payTo), so a lenient `exact` reader
    /// accepts it. Paying it with an exact transfer would be rejected by a
    /// server that never advertised that scheme, so classification must prefer
    /// the batch reading — this pins that ordering.
    #[test]
    fn classify_402_prefers_batch_settlement_over_a_lenient_exact_reading() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;

        let envelope = serde_json::json!({
            "x402Version": 2,
            "accepts": [{
                "scheme": "batch-settlement",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "1000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY",
                "maxTimeoutSeconds": 300,
                "extra": {
                    "feePayer": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
                    "withdrawDelay": 3600,
                    "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
                }
            }]
        });
        let headers = vec![(
            "payment-required".to_string(),
            STANDARD.encode(serde_json::to_vec(&envelope).unwrap()),
        )];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(
            matches!(outcome, RunOutcome::X402BatchChallenge { .. }),
            "expected a batch-settlement challenge, got: {outcome:?}"
        );
    }

    /// The corrective 402 the server sends on a cumulative mismatch must reach
    /// the client as a batch challenge carrying its error code — that is what
    /// lets the client resynchronize instead of retrying a stale amount.
    #[test]
    fn classify_402_surfaces_the_corrective_batch_error() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;

        let envelope = serde_json::json!({
            "x402Version": 2,
            "error": "invalid_batch_settlement_svm_cumulative_amount_mismatch",
            "accepts": [{
                "scheme": "batch-settlement",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "1000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY",
                "maxTimeoutSeconds": 300,
                "extra": {
                    "feePayer": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
                    "withdrawDelay": 3600,
                    "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                    "channelState": {
                        "channelId": "Chan11111111111111111111111111111111111111",
                        "balance": "100000",
                        "totalClaimed": "2000",
                        "withdrawRequestedAt": 0,
                        "chargedCumulativeAmount": "3000"
                    }
                }
            }]
        });
        let headers = vec![(
            "payment-required".to_string(),
            STANDARD.encode(serde_json::to_vec(&envelope).unwrap()),
        )];

        let RunOutcome::X402BatchChallenge { challenge, .. } =
            classify_402(&headers, None, "https://example.com/resource")
        else {
            panic!("expected a batch-settlement challenge");
        };
        assert_eq!(
            challenge.error.as_deref(),
            Some("invalid_batch_settlement_svm_cumulative_amount_mismatch")
        );
        assert!(challenge.requirements.is_corrective());
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
            pay_kit::x402::X402_V1_PAYMENT_REQUIRED_HEADER.to_string(),
            requirements.to_string(),
        )];

        let outcome = classify_402(&headers, None, "https://example.com/resource");
        assert!(matches!(outcome, RunOutcome::X402Challenge { .. }));
    }

    /// Build a 402 with BOTH an MPP charge challenge AND an x402 header,
    /// so `classify_402_with_preference` has a real choice between the
    /// two protocols. Shared by the `--mpp` / `--x402` preference tests.
    fn dual_protocol_402() -> Vec<(String, String)> {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let mpp_request = serde_json::json!({
            "amount": "1000000",
            "currency": "USDC",
            "recipient": "So11111111111111111111111111111111111111112",
            "methodDetails": { "network": "devnet" }
        });
        let mpp_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&mpp_request).unwrap());

        let x402_requirements = serde_json::json!({
            "network": "solana",
            "cluster": "devnet",
            "recipient": "So11111111111111111111111111111111111111112",
            "amount": "1000000",
            "currency": "USDC",
            "resource": "https://example.com/resource"
        });

        vec![
            (
                "www-authenticate".to_string(),
                format!(
                    "Payment id=\"dual\", realm=\"test\", method=\"solana\", intent=\"charge\", request=\"{mpp_b64}\""
                ),
            ),
            (
                pay_kit::x402::X402_V1_PAYMENT_REQUIRED_HEADER.to_string(),
                x402_requirements.to_string(),
            ),
        ]
    }

    /// A 402 shaped like the Gemini gateway's: an operator-signed session
    /// and a flat charge for the same price, both MPP over Solana.
    fn session_and_charge_402(voucher_signer: &str) -> Vec<(String, String)> {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let session_request = serde_json::json!({
            "amount": "45000",
            "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "recipient": "BcdwLA62UPEAvRn7AWauMUXKtYMXxdLzTPaSQg5tNaFc",
            "suggestedDeposit": "250000",
            "methodDetails": {
                "network": "mainnet",
                "channelProgram": "CHNLxYvVA28MJP9PrFuDXccuoGXAx7jBacfLEkahyGsX",
                "operator": "BcdwLA62UPEAvRn7AWauMUXKtYMXxdLzTPaSQg5tNaFc",
                "voucherSigner": voucher_signer,
                "minVoucherDelta": "1"
            }
        });
        let charge_request = serde_json::json!({
            "amount": "45000",
            "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "recipient": "Cs2zdfUNonRdRGsiZUQQLdTxzxVvJZmgiX2mpLYKuEqP",
            "methodDetails": { "network": "mainnet" }
        });
        let encode = |v: &serde_json::Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        vec![
            (
                "www-authenticate".to_string(),
                format!(
                    "Payment id=\"s\", realm=\"test\", method=\"solana\", intent=\"session\", request=\"{}\"",
                    encode(&session_request)
                ),
            ),
            (
                "www-authenticate".to_string(),
                format!(
                    "Payment id=\"c\", realm=\"test\", method=\"solana\", intent=\"charge\", request=\"{}\"",
                    encode(&charge_request)
                ),
            ),
        ]
    }

    #[test]
    fn session_challenge_carries_the_charge_from_the_same_402_as_fallback() {
        let outcome = classify_402(&session_and_charge_402("operator"), None, "https://e.com/r");
        let RunOutcome::SessionChallenge { fallback, .. } = outcome else {
            panic!("session is preferred when offered: {outcome:?}");
        };
        let fallback = fallback.expect("the charge beside the session is the fallback");
        assert!(
            matches!(*fallback, RunOutcome::MppChallenge { .. }),
            "{fallback:?}"
        );
    }

    #[test]
    fn session_challenge_carries_x402_fallback_when_that_is_the_other_offer() {
        let outcome = classify_402(&dual_protocol_session_402(), None, "https://e.com/r");
        let RunOutcome::SessionChallenge { fallback, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert!(matches!(
            fallback.as_deref(),
            Some(RunOutcome::X402Challenge { .. })
        ));
    }

    #[test]
    fn session_only_402_has_no_fallback() {
        let mut headers = session_and_charge_402("operator");
        headers.truncate(1);
        let outcome = classify_402(&headers, None, "https://e.com/r");
        let RunOutcome::SessionChallenge { fallback, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert!(fallback.is_none());
    }

    #[test]
    fn for_signer_keeps_the_session_for_a_signer_that_signs_raw_messages() {
        use crate::backend::testing::SignsAnything;
        let outcome = classify_402(&session_and_charge_402("operator"), None, "https://e.com/r")
            .for_signer(&SignsAnything);
        assert!(matches!(outcome, RunOutcome::SessionChallenge { .. }));
    }

    #[test]
    fn for_signer_takes_the_charge_when_the_session_needs_a_raw_signature() {
        use crate::backend::testing::TransactionsOnly;
        let outcome = classify_402(&session_and_charge_402("operator"), None, "https://e.com/r")
            .for_signer(&TransactionsOnly);
        let RunOutcome::MppChallenge { challenge, .. } = outcome else {
            panic!("a transactions-only signer must pay the charge: {outcome:?}");
        };
        assert_eq!(challenge.intent.as_str(), "charge");
    }

    #[test]
    fn for_signer_keeps_a_client_signed_session_for_a_hardware_signer() {
        use crate::backend::testing::TransactionsOnly;
        let outcome = classify_402(&session_and_charge_402("client"), None, "https://e.com/r")
            .for_signer(&TransactionsOnly);
        assert!(matches!(outcome, RunOutcome::SessionChallenge { .. }));
    }

    #[test]
    fn for_signer_rejects_with_the_reason_when_nothing_else_is_offered() {
        use crate::backend::testing::TransactionsOnly;
        let mut headers = session_and_charge_402("operator");
        headers.truncate(1);
        let outcome = classify_402(&headers, None, "https://e.com/r").for_signer(&TransactionsOnly);
        let RunOutcome::PaymentRejected {
            reason, retryable, ..
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert!(!retryable);
        assert!(
            reason.starts_with("Test hardware wallet cannot sign an operator-signed session proof"),
            "{reason}"
        );
        assert!(
            reason.ends_with("This endpoint offers no other payment option."),
            "{reason}"
        );
    }

    #[test]
    fn for_signer_pays_instead_of_signing_in_when_the_signer_cannot_sign_in() {
        use crate::backend::testing::{SignsAnything, TransactionsOnly};
        let headers = siwx_with_payment_402();
        let signed_in = classify_402(&headers, None, "https://e.com/r").for_signer(&SignsAnything);
        assert!(
            matches!(signed_in, RunOutcome::X402SignInChallenge { .. }),
            "{signed_in:?}"
        );
        let paid = classify_402(&headers, None, "https://e.com/r").for_signer(&TransactionsOnly);
        assert!(matches!(paid, RunOutcome::X402Challenge { .. }), "{paid:?}");
    }

    #[test]
    fn for_signer_leaves_other_outcomes_alone() {
        use crate::backend::testing::TransactionsOnly;
        let outcome = classify_402(&dual_protocol_402(), None, "https://e.com/r");
        assert!(matches!(outcome, RunOutcome::MppChallenge { .. }));
        let same = outcome.for_signer(&TransactionsOnly);
        assert!(matches!(same, RunOutcome::MppChallenge { .. }));
    }

    #[test]
    fn configured_signer_passes_through_when_a_throwaway_wallet_would_pay() {
        // No account on an ephemeral network: a software wallet is created
        // at payment time and signs anything, so the session stands.
        let store = crate::accounts::MemoryAccountsStore::new();
        let outcome = classify_402(&session_and_charge_402("operator"), None, "https://e.com/r");
        let support = outcome
            .configured_signer_support(&store, Some("localnet"), None)
            .unwrap();
        let outcome = outcome.for_signer_support(support);
        assert!(
            matches!(outcome, RunOutcome::SessionChallenge { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn configured_signer_uses_the_network_the_offer_names() {
        // Mainnet with nothing configured is an error at payment time; the
        // pre-check reports the same thing rather than guessing a wallet.
        let store = crate::accounts::MemoryAccountsStore::new();
        let outcome = classify_402(&session_and_charge_402("operator"), None, "https://e.com/r");
        let Err(err) = outcome.configured_signer_support(&store, None, None) else {
            panic!("mainnet without an account is an error");
        };
        assert!(
            err.to_string()
                .contains("No account configured for network `mainnet`"),
            "{err}"
        );
    }

    #[cfg(feature = "ledger")]
    #[test]
    fn configured_signer_lets_a_ledger_account_pay_the_charge() {
        let mut file = crate::accounts::AccountsFile::default();
        file.upsert(
            "mainnet",
            "ledger",
            crate::accounts::Account {
                backend: crate::accounts::BackendKind::Remote,
                provider: Some("ledger".to_string()),
                active: true,
                auth_required: Some(true),
                pubkey: Some("CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z".to_string()),
                vault: None,
                account: Some("m/44'/501'/0'".to_string()),
                path: None,
                secret_key_b58: None,
                created_at: None,
                subscriptions: Default::default(),
            },
        );
        let store = crate::accounts::MemoryAccountsStore::with_file(file);
        let outcome = classify_402(&session_and_charge_402("operator"), None, "https://e.com/r");
        let support = outcome
            .configured_signer_support(&store, None, None)
            .unwrap();
        let outcome = outcome.for_signer_support(support);
        assert!(matches!(outcome, RunOutcome::MppChallenge { .. }));
    }

    /// An x402 402 offering sign-in-with-x beside an exact payment.
    fn siwx_with_payment_402() -> Vec<(String, String)> {
        use base64::Engine;
        let payment_required = serde_json::json!({
            pay_kit::x402::X402_VERSION_FIELD: pay_kit::x402::X402_VERSION_V2,
            "resource": { "url": "https://e.com/r", "description": "API access" },
            "accepts": [{
                "scheme": "exact",
                "network": pay_kit::x402::exact::SOLANA_MAINNET,
                "amount": "1000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "Cs2zdfUNonRdRGsiZUQQLdTxzxVvJZmgiX2mpLYKuEqP",
                "maxTimeoutSeconds": 60,
                "extra": { "feePayer": "BcdwLA62UPEAvRn7AWauMUXKtYMXxdLzTPaSQg5tNaFc" }
            }],
            "extensions": {
                "sign-in-with-x": {
                    "info": {
                        "domain": "e.com",
                        "uri": "https://e.com",
                        "version": "1",
                        "nonce": "nonce-123",
                        "issuedAt": "2026-04-27T00:00:00Z"
                    },
                    "supportedChains": [{
                        "chainId": pay_kit::x402::exact::SOLANA_MAINNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        });
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(payment_required.to_string().as_bytes());
        vec![(pay_kit::x402::PAYMENT_REQUIRED_HEADER.to_string(), encoded)]
    }

    fn dual_protocol_session_402() -> Vec<(String, String)> {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let session_request = serde_json::json!({
            "cap": "1000000",
            "currency": "USDC",
            "network": "devnet",
            "operator": "So11111111111111111111111111111111111111112",
            "recipient": "So11111111111111111111111111111111111111112",
            "modes": ["pull"]
        });
        let session_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&session_request).unwrap());
        let mut headers = dual_protocol_402();
        headers[0].1 = format!(
            "Payment id=\"dual-session\", realm=\"test\", method=\"solana\", \
             intent=\"session\", request=\"{session_b64}\""
        );
        headers
    }

    #[test]
    fn decoded_challenges_group_mpp_and_x402_before_protocol_selection() {
        let headers = dual_protocol_402();
        let decoded = decode_payment_challenges(&headers, None);

        assert_eq!(decoded.mpp.len(), 1);
        assert_eq!(decoded.x402.len(), 1);
        assert_eq!(decoded.mpp[0]["intent"], "charge");
        assert_eq!(decoded.mpp[0]["request"]["currency"], "USDC");
        assert_eq!(decoded.x402[0]["currency"], "USDC");
        assert_eq!(decoded.x402[0]["cluster"], "devnet");
    }

    #[test]
    fn decoded_challenges_preserve_every_x402_accept() {
        use base64::Engine;

        let envelope = serde_json::json!({
            "x402Version": 2,
            "accepts": [
                {
                    "scheme": "exact",
                    "network": "solana:mainnet",
                    "amount": "10",
                    "asset": "USDC",
                    "payTo": "exact-recipient"
                },
                {
                    "scheme": "upto",
                    "network": "solana:mainnet",
                    "amount": "250000",
                    "asset": "USDC",
                    "payTo": "upto-recipient"
                }
            ]
        });
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&envelope).unwrap());
        let headers = vec![(
            pay_kit::x402::PAYMENT_REQUIRED_HEADER.to_ascii_lowercase(),
            encoded,
        )];

        let decoded = decode_payment_challenges(&headers, None);
        assert_eq!(decoded.x402.len(), 2);
        assert_eq!(decoded.x402[0]["scheme"], "exact");
        assert_eq!(decoded.x402[1]["scheme"], "upto");
    }

    #[test]
    fn decoded_challenges_preserve_siwx_only_envelope() {
        use base64::Engine;

        let envelope = serde_json::json!({
            "x402Version": 2,
            "accepts": [],
            "extensions": {
                "sign-in-with-x": {
                    "info": {
                        "domain": "example.com",
                        "nonce": "nonce-123"
                    }
                }
            }
        });
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&envelope).unwrap());
        let headers = vec![(
            pay_kit::x402::PAYMENT_REQUIRED_HEADER.to_ascii_lowercase(),
            encoded,
        )];

        let decoded = decode_payment_challenges(&headers, None);
        assert_eq!(decoded.x402, vec![envelope]);
    }

    #[test]
    fn preference_auto_prefers_mpp_when_both_offered() {
        let outcome = classify_402_with_preference(
            &dual_protocol_402(),
            None,
            "https://example.com/resource",
            ProtocolPreference::Auto,
        );
        assert!(matches!(outcome, RunOutcome::MppChallenge { .. }));
    }

    #[test]
    fn preference_only_x402_picks_x402_when_both_offered() {
        let outcome = classify_402_with_preference(
            &dual_protocol_402(),
            None,
            "https://example.com/resource",
            ProtocolPreference::OnlyX402,
        );
        assert!(matches!(outcome, RunOutcome::X402Challenge { .. }));
    }

    #[test]
    fn preference_only_x402_skips_mpp_session_when_both_offered() {
        let outcome = classify_402_with_preference(
            &dual_protocol_session_402(),
            None,
            "https://example.com/resource",
            ProtocolPreference::OnlyX402,
        );
        assert!(matches!(outcome, RunOutcome::X402Challenge { .. }));
    }

    #[test]
    fn preference_auto_still_prefers_mpp_session_when_both_offered() {
        let outcome = classify_402_with_preference(
            &dual_protocol_session_402(),
            None,
            "https://example.com/resource",
            ProtocolPreference::Auto,
        );
        assert!(matches!(outcome, RunOutcome::SessionChallenge { .. }));
    }

    #[test]
    fn preference_only_mpp_keeps_mpp_when_both_offered() {
        let outcome = classify_402_with_preference(
            &dual_protocol_402(),
            None,
            "https://example.com/resource",
            ProtocolPreference::OnlyMpp,
        );
        assert!(matches!(outcome, RunOutcome::MppChallenge { .. }));
    }

    #[test]
    fn preference_only_x402_rejects_when_server_only_offers_mpp() {
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
                "Payment id=\"mpp-only\", realm=\"test\", method=\"solana\", intent=\"charge\", request=\"{b64}\""
            ),
        )];

        let outcome = classify_402_with_preference(
            &headers,
            None,
            "https://example.com/resource",
            ProtocolPreference::OnlyX402,
        );
        match outcome {
            RunOutcome::PaymentRejected { reason, .. } => {
                assert!(reason.contains("--x402"), "reason was {reason:?}");
                assert!(reason.contains("MPP"), "reason was {reason:?}");
            }
            other => panic!("expected PaymentRejected, got {other:?}"),
        }
    }

    #[test]
    fn preference_only_mpp_rejects_when_server_only_offers_x402() {
        let requirements = serde_json::json!({
            "network": "solana",
            "cluster": "devnet",
            "recipient": "So11111111111111111111111111111111111111112",
            "amount": "1000000",
            "currency": "USDC",
            "resource": "https://example.com/resource"
        });
        let headers = vec![(
            pay_kit::x402::X402_V1_PAYMENT_REQUIRED_HEADER.to_string(),
            requirements.to_string(),
        )];

        let outcome = classify_402_with_preference(
            &headers,
            None,
            "https://example.com/resource",
            ProtocolPreference::OnlyMpp,
        );
        match outcome {
            RunOutcome::PaymentRejected { reason, .. } => {
                assert!(reason.contains("--mpp"), "reason was {reason:?}");
                assert!(reason.contains("x402"), "reason was {reason:?}");
            }
            other => panic!("expected PaymentRejected, got {other:?}"),
        }
    }

    #[test]
    fn classify_402_with_x402_siwx_auth_only_header() {
        use base64::Engine;

        let payment_required = serde_json::json!({
            pay_kit::x402::X402_VERSION_FIELD: pay_kit::x402::X402_VERSION_V2,
            "resource": {
                "url": "https://example.com/resource",
                "description": "API access"
            },
            "accepts": [],
            "extensions": {
                "sign-in-with-x": {
                    "info": {
                        "domain": "example.com",
                        "uri": "https://example.com",
                        "version": "1",
                        "nonce": "nonce-123",
                        "issuedAt": "2026-04-27T00:00:00Z"
                    },
                    "supportedChains": [{
                        "chainId": pay_kit::x402::exact::SOLANA_MAINNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        });
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(payment_required.to_string().as_bytes());
        let headers = vec![(pay_kit::x402::PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        let outcome = classify_402(&headers, None, "https://example.com/resource");

        match outcome {
            RunOutcome::X402SignInChallenge {
                challenge,
                resource_url,
                ..
            } => {
                assert_eq!(challenge.extension.nonce, "nonce-123");
                assert_eq!(resource_url, "https://example.com/resource");
            }
            other => panic!("expected X402SignInChallenge, got {other:?}"),
        }
    }

    #[test]
    fn classify_402_prefers_signin_with_payment_fallback() {
        use base64::Engine;

        let selected = serde_json::json!({
            "scheme": pay_kit::x402::exact::EXACT_SCHEME,
            "network": pay_kit::x402::exact::SOLANA_MAINNET,
            "amount": "10000",
            "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "payTo": "6cvgmdrsVxyiuPzqMCSBnS7fAmA5Mk2VG4BcfVhC8jdC",
            "maxTimeoutSeconds": 300
        });
        let payment_required = serde_json::json!({
            pay_kit::x402::X402_VERSION_FIELD: pay_kit::x402::X402_VERSION_V2,
            "accepts": [selected],
            "extensions": {
                "sign-in-with-x": {
                    "info": {
                        "domain": "example.com",
                        "uri": "https://example.com",
                        "version": "1",
                        "nonce": "nonce-123",
                        "issuedAt": "2026-04-27T00:00:00Z"
                    },
                    "supportedChains": [{
                        "chainId": pay_kit::x402::exact::SOLANA_MAINNET,
                        "type": "ed25519",
                        "signatureScheme": "siws"
                    }]
                }
            }
        });
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(payment_required.to_string().as_bytes());
        let headers = vec![(pay_kit::x402::PAYMENT_REQUIRED_HEADER.to_string(), encoded)];

        let outcome = classify_402(&headers, None, "https://example.com/resource");

        // Prefer sign-in (spend credits) over paying when both are offered,
        // but keep the payment option as a fallback for the no-credits case.
        match outcome {
            RunOutcome::X402SignInChallenge {
                challenge,
                payment_fallback,
                ..
            } => {
                assert_eq!(challenge.extension.nonce, "nonce-123");
                let pay = payment_fallback.expect("payment option preserved as fallback");
                assert_eq!(pay.requirements.amount, "10000");
            }
            other => panic!("expected X402SignInChallenge, got {other:?}"),
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
    fn parse_verification_failure_accepts_session_failure() {
        let body = r#"{"error":"session_failed","message":"open transaction not visible","retryable":true}"#;
        let parsed = parse_verification_failure(Some(body));
        assert_eq!(
            parsed,
            Some(("open transaction not visible".to_string(), true))
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
    fn parse_httpie_history_is_not_reparsed_from_response_body() {
        let raw = "HTTP/1.1 302 Found\nLocation: https://example.com/final\n\nredirecting\nHTTP/1.1 200 OK\nContent-Type: application/json\nPayment-Response: encoded-settlement\n\n{\"ok\":true}";
        let (status, headers, body) = parse_httpie_output(raw);

        assert_eq!(status, Some(302));
        assert!(
            headers.iter().any(|(name, value)| {
                name == "location" && value == "https://example.com/final"
            })
        );
        assert!(!headers.iter().any(|(name, _)| name == "payment-response"));
        assert_eq!(
            body.as_deref(),
            Some(
                "redirecting\nHTTP/1.1 200 OK\nContent-Type: application/json\nPayment-Response: encoded-settlement\n\n{\"ok\":true}"
            )
        );
    }

    #[test]
    fn parse_httpie_200_body_cannot_inject_payment_required_response() {
        let raw = "HTTP/1.1 200 OK\nContent-Type: text/plain\n\nharmless body\nHTTP/1.1 402 Payment Required\nPayment-Required: attacker-controlled-challenge\n\ncharge me";
        let (status, headers, body) = parse_httpie_output(raw);

        assert_eq!(status, Some(200));
        assert_eq!(headers, vec![("content-type".into(), "text/plain".into())]);
        assert_eq!(
            body.as_deref(),
            Some(
                "harmless body\nHTTP/1.1 402 Payment Required\nPayment-Required: attacker-controlled-challenge\n\ncharge me"
            )
        );
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
