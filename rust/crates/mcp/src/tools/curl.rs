use base64::{Engine, engine::general_purpose};
use rmcp::model::{CallToolResult, Content};
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    #[schemars(description = "The URL to fetch (e.g. https://api.example.com/data)")]
    pub url: String,
    #[schemars(description = "HTTP method. Defaults to GET.")]
    pub method: Option<String>,
    #[schemars(
        description = "Request headers as key-value pairs (e.g. {\"Authorization\": \"Bearer token\"})"
    )]
    pub headers: Option<std::collections::HashMap<String, String>>,
    #[schemars(
        description = "Request body for POST/PUT/PATCH. Pass either a string or a JSON value; JSON values are serialized before sending and validated locally against cached Pay catalog OpenAPI schemas when available."
    )]
    pub body: Option<BodyParam>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum BodyParam {
    Text(String),
    Json(Value),
}

impl BodyParam {
    fn into_string(self) -> Result<String, serde_json::Error> {
        match self {
            Self::Text(body) => Ok(body),
            Self::Json(value) => serde_json::to_string(&value),
        }
    }
}

/// Prepare request headers from params — auto-injects Accept and Content-Type.
pub fn prepare_headers(
    user_headers: &Option<std::collections::HashMap<String, String>>,
    has_body: bool,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(h) = user_headers {
        for (k, v) in h {
            headers.push((k.clone(), v.clone()));
        }
    }
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("accept"))
    {
        headers.push(("Accept".to_string(), "application/json".to_string()));
    }
    if has_body
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    headers
}

pub async fn run(
    params: Params,
    peer: rmcp::Peer<rmcp::service::RoleServer>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let headers = prepare_headers(&params.headers, params.body.is_some());
    let method = params.method.clone().unwrap_or_else(|| "GET".to_string());
    let body = match params.body.clone().map(BodyParam::into_string).transpose() {
        Ok(body) => body,
        Err(err) => {
            return Ok(super::tool_error(format!(
                "Failed to serialize request body: {err}"
            )));
        }
    };
    let url = params.url.clone();

    let response = tokio::task::spawn_blocking(move || {
        do_paid_fetch(&method, &url, &headers, body, Some(peer))
    })
    .await
    .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    match response {
        Ok((body, content_type)) => Ok(CallToolResult::success(body_to_mcp_content(
            body,
            content_type.as_deref(),
            "Request completed.",
        ))),
        Err(err) => Ok(pay_error_to_tool_result(err)),
    }
}

/// Route a response body to the right MCP content kind based on its MIME type.
///
/// - `image/*` → base64-encoded `Content::image` (so the LLM can see it)
/// - other binary (`application/pdf`, `application/octet-stream`, etc.) →
///   spilled to a tempfile, response carries the path as `Content::text`
///   (the JSON-RPC transport mangles raw bytes; tempfile keeps them intact)
/// - text-typed (`text/*`, `application/json`, `application/xml`) →
///   `Content::text` with UTF-8 lossy decode
/// - empty body → `Content::text(empty_message)`
fn body_to_mcp_content(
    body: Vec<u8>,
    content_type: Option<&str>,
    empty_message: &str,
) -> Vec<Content> {
    if body.is_empty() {
        return vec![Content::text(empty_message.to_string())];
    }

    let mime = mime_from_content_type(content_type);

    if mime.starts_with("image/") {
        let encoded = general_purpose::STANDARD.encode(&body);
        return vec![Content::image(encoded, mime)];
    }

    if is_binary_content_type(&mime) {
        match write_body_to_tempfile(&body, &mime) {
            Ok(path) => vec![Content::text(format!(
                "Binary response ({} bytes, {mime}) written to {path}",
                body.len()
            ))],
            Err(err) => vec![Content::text(format!(
                "Binary response ({} bytes, {mime}) — failed to spill to tempfile: {err}",
                body.len()
            ))],
        }
    } else {
        vec![Content::text(String::from_utf8_lossy(&body).into_owned())]
    }
}

fn mime_from_content_type(content_type: Option<&str>) -> String {
    content_type
        .and_then(|v| v.split(';').next())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// True for MIME types whose payloads are not safe to embed as UTF-8 text.
/// Text-typed MIMEs (`text/*`, `application/json`, `application/xml`,
/// `application/*+json`, `application/*+xml`) return false.
fn is_binary_content_type(mime: &str) -> bool {
    if mime.starts_with("text/") {
        return false;
    }
    if mime == "application/json" || mime == "application/xml" {
        return false;
    }
    if mime.starts_with("application/") && (mime.ends_with("+json") || mime.ends_with("+xml")) {
        return false;
    }
    true
}

fn write_body_to_tempfile(body: &[u8], mime: &str) -> std::io::Result<String> {
    use std::io::Write;
    let extension = extension_for_mime(mime);
    let mut path = std::env::temp_dir();
    let name = format!(
        "pay-curl-{}{extension}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    path.push(name);
    let mut file = std::fs::File::create(&path)?;
    file.write_all(body)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Pick a sensible filename extension for a MIME type using `mime_guess`'s
/// MIME→ext map. The extension is purely a hint for the human reading the
/// tempfile path — readers should always trust `Content-Type`, not the
/// suffix.
fn extension_for_mime(mime: &str) -> String {
    let parsed: Option<mime_guess::Mime> = mime.parse().ok();
    parsed
        .as_ref()
        .and_then(|m| mime_guess::get_mime_extensions(m))
        .and_then(|exts| exts.first())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_else(|| ".bin".to_string())
}

/// Result of a paid fetch: raw response body bytes and the content-type the
/// server advertised. Bytes (not String) so binary payloads — images, PDFs,
/// octet streams — round-trip without UTF-8 mangling.
type PaidFetchResult = (Vec<u8>, Option<String>);

fn do_paid_fetch(
    method: &str,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<String>,
    peer: Option<rmcp::Peer<rmcp::service::RoleServer>>,
) -> Result<PaidFetchResult, pay_core::Error> {
    use pay_core::client::runner::RunOutcome;

    pay_core::skills::validate_cached_catalog_request(method, url, body.as_deref())?;

    // Build a fresh elicitation-backed AuthGate per signing operation when
    // we have a peer AND no local biometric is available. A local Touch ID /
    // Windows Hello / polkit prompt is faster and more familiar than a
    // round-trip through the MCP client UI, so we prefer it whenever the
    // platform offers it. `PAY_FORCE_ELICITATION=1` opts back into the
    // elicitation path for users who want approvals in the MCP client
    // anyway (remote MCP, screen-sharing demos, etc.).
    //
    // When None (e.g. unit tests, or biometrics-available path), each
    // `_with_override` call gets `None` and falls back to the platform
    // default gate. The peer is cheap to clone (it wraps an Arc).
    let make_auth_override = || -> pay_core::signer::AuthOverride {
        let peer = peer.as_ref()?;
        let force = std::env::var("PAY_FORCE_ELICITATION")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !force && pay_keystore::Keystore::any_biometric_available() {
            return None;
        }
        Some(Box::new(crate::ElicitationAuth::new(peer.clone())) as Box<dyn pay_keystore::AuthGate>)
    };

    let store = pay_core::accounts::FileAccountsStore::default_path();
    let network_override = std::env::var("PAY_NETWORK_ENFORCED").ok();
    let account_override = std::env::var("PAY_ACTIVE_ACCOUNT").ok();

    // SIWMPP pre-attach: if a cached authenticate token covers this URL
    // (URL-prefix match against a tracked Active subscription with a
    // non-expired token), attach it BEFORE the first fetch. On hit the
    // server validates the token and skips the 402 entirely — no Touch
    // ID prompt, no extra round trip. On miss this is a no-op.
    let cached_auth_header =
        pay_core::client::authenticate::cached_header_for_resource(&store, url);
    let initial_headers: Vec<(String, String)> = match cached_auth_header.as_deref() {
        Some(token)
            if !extra_headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization")) =>
        {
            let mut h = extra_headers.to_vec();
            h.push(("Authorization".to_string(), token.to_string()));
            h
        }
        _ => extra_headers.to_vec(),
    };

    let outcome =
        pay_core::client::fetch::fetch_request(method, url, &initial_headers, body.as_deref())?;

    match outcome {
        RunOutcome::MppChallenge {
            challenge,
            alternatives,
            ..
        } => {
            let mut challenges = Vec::with_capacity(1 + alternatives.len());
            challenges.push((*challenge).clone());
            challenges.extend(alternatives);
            let selected = pay_core::client::mpp::select_challenge_by_balance(
                &challenges,
                &store,
                network_override.as_deref(),
                account_override.as_deref(),
            )?
            .ok_or_else(|| pay_core::Error::Mpp("No compatible MPP challenge found".to_string()))?;
            let (auth_header, _ephemeral) = pay_core::client::mpp::build_credential_with_override(
                selected,
                &store,
                network_override.as_deref(),
                account_override.as_deref(),
                Some(url),
                make_auth_override(),
            )?;
            let mut headers = extra_headers.to_vec();
            headers.push(("Authorization".to_string(), auth_header));
            interpret_retry(pay_core::client::fetch::fetch_request(
                method,
                url,
                &headers,
                body.as_deref(),
            )?)
        }
        RunOutcome::X402Challenge { challenge, .. } => {
            let built_payment = pay_core::client::x402::build_payment_with_override(
                &challenge,
                &store,
                network_override.as_deref(),
                account_override.as_deref(),
                Some(url),
                make_auth_override(),
            )?;
            let mut headers = extra_headers.to_vec();
            headers.extend(
                built_payment
                    .headers
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), value)),
            );
            interpret_retry(pay_core::client::fetch::fetch_request(
                method,
                url,
                &headers,
                body.as_deref(),
            )?)
        }
        RunOutcome::X402SignInChallenge {
            challenge,
            payment_fallback,
            ..
        } => {
            // Prefer spending existing credits: sign in with the wallet and
            // retry. The sign-in signature takes one Touch ID / approval; if
            // sign-in doesn't grant access and we fall back to paying below,
            // the payment signature requires a second approval.
            let built = pay_core::client::x402::build_siwx_auth_header_with_override(
                &challenge,
                &store,
                network_override.as_deref(),
                account_override.as_deref(),
                Some(url),
                make_auth_override(),
            )?;
            let mut headers = extra_headers.to_vec();
            headers.extend(
                built
                    .headers
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), value)),
            );
            let retry =
                pay_core::client::fetch::fetch_request(method, url, &headers, body.as_deref())?;

            // If sign-in granted access, we're done — credits were spent, no
            // payment made. If the server still refuses (e.g. the wallet has
            // no credits yet) and the same 402 also offered a payment option,
            // fall back to paying so the call can still go through.
            if matches!(retry, RunOutcome::Completed { .. }) {
                interpret_retry(retry)
            } else if let Some(pay_challenge) = payment_fallback {
                let built_payment = pay_core::client::x402::build_payment_with_override(
                    &pay_challenge,
                    &store,
                    network_override.as_deref(),
                    account_override.as_deref(),
                    Some(url),
                    make_auth_override(),
                )?;
                let mut headers = extra_headers.to_vec();
                headers.extend(
                    built_payment
                        .headers
                        .into_iter()
                        .map(|(name, value)| (name.to_string(), value)),
                );
                interpret_retry(pay_core::client::fetch::fetch_request(
                    method,
                    url,
                    &headers,
                    body.as_deref(),
                )?)
            } else if let RunOutcome::PaymentRejected { reason, .. } = retry {
                Err(pay_core::Error::PaymentRejected(reason))
            } else {
                // Sign-in didn't grant access and the 402 offered no payment
                // option to fall back to — typically the wallet has no credits
                // yet. Don't claim a payment was made/rejected here.
                Err(pay_core::Error::Mpp(
                    "Server returned 402 again after sign-in — the wallet has no usable credits \
                     and the endpoint offered no payment option"
                        .to_string(),
                ))
            }
        }
        RunOutcome::SessionChallenge { .. } => Err(pay_core::Error::Mpp(
            "402 Payment Required (MPP session) — session payments require a stateful client with a Fiber channel".to_string(),
        )),
        RunOutcome::SubscriptionChallenge {
            challenge,
            authenticate,
            ..
        } => {
            // Build + send the activation credential, same flow as
            // `pay http`. Touch ID (or whatever keystore the active
            // account uses) gates the signature, so an agent invocation
            // can't silently commit a recurring on-chain delegation —
            // the user still has to approve in the system prompt. On
            // success we persist a local record to accounts.yml via
            // pay-core's shared helper so the MCP path stays in sync
            // with `pay subscriptions list`. When the server bundled an
            // `authenticate` challenge in the 402, we sign it with the
            // same unlocked signer and cache the resulting token so
            // subsequent requests in the period skip the prompt.
            let built =
                pay_core::client::subscription::build_credential_with_authenticate_and_override(
                    &challenge,
                    authenticate.as_deref(),
                    &store,
                    network_override.as_deref(),
                    account_override.as_deref(),
                    Some(url),
                    make_auth_override(),
                )?;
            let mut headers = extra_headers.to_vec();
            headers.push(("Authorization".to_string(), built.authorization.clone()));
            let retry = pay_core::client::fetch::fetch_request(
                method,
                url,
                &headers,
                body.as_deref(),
            )?;
            if let RunOutcome::Completed { exit_code, .. } = &retry
                && *exit_code == 0
                && let Err(e) =
                    pay_core::client::subscription::persist_local_subscription_after_activation(
                        &built, &store,
                    )
            {
                tracing::warn!(
                    error = %e,
                    "Subscription activation succeeded but local persistence failed"
                );
            }
            interpret_retry(retry)
        }
        RunOutcome::PaymentRejected { reason, .. } => Err(pay_core::Error::PaymentRejected(reason)),
        RunOutcome::UnknownPaymentRequired { .. } => Err(pay_core::Error::Mpp(
            "402 Payment Required but no recognized protocol".to_string(),
        )),
        RunOutcome::Completed {
            body,
            content_type,
            ..
        } => Ok((body.unwrap_or_default(), content_type)),
    }
}

fn pay_error_to_tool_result(err: pay_core::Error) -> CallToolResult {
    let message = match err {
        pay_core::Error::RequestValidation(message) => message,
        pay_core::Error::PaymentRejected(reason) if is_user_rejection(&reason) => {
            format!(
                "User declined the OS authentication prompt for this paid request: {reason}. \
                 The HTTP request was NOT sent and no funds moved. Ask the user for \
                 clarification before retrying — they may have intended to decline (in which \
                 case clarify what to do instead), or they may want to retry and approve at \
                 the prompt."
            )
        }
        other => format!("Pay curl failed: {other}"),
    };
    super::tool_error(message)
}

/// True when a `PaymentRejected` reason came from the user denying their OS
/// auth prompt (Apple Keychain, Windows Hello, GNOME Keyring, 1Password, or
/// the generic fallback) — not from a server-side `verification_failed` body.
/// See `signer::rejection_source` for the matching producer.
fn is_user_rejection(reason: &str) -> bool {
    reason.starts_with("rejected by user")
}

fn interpret_retry(
    outcome: pay_core::client::runner::RunOutcome,
) -> Result<PaidFetchResult, pay_core::Error> {
    use pay_core::client::runner::RunOutcome;
    match outcome {
        RunOutcome::Completed {
            body, content_type, ..
        } => Ok((body.unwrap_or_default(), content_type)),
        RunOutcome::PaymentRejected { reason, .. } => Err(pay_core::Error::PaymentRejected(reason)),
        _ => Err(pay_core::Error::Mpp(
            "Server returned 402 again after payment".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_deserialize_minimal() {
        let json = r#"{"url": "https://example.com"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.url, "https://example.com");
        assert!(params.method.is_none());
        assert!(params.headers.is_none());
        assert!(params.body.is_none());
    }

    #[test]
    fn params_deserialize_full() {
        let json = r#"{
            "url": "https://example.com",
            "method": "POST",
            "headers": {"Authorization": "Bearer tok"},
            "body": "{\"q\":1}"
        }"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.method.unwrap(), "POST");
        assert_eq!(params.headers.as_ref().unwrap().len(), 1);
        assert!(params.body.is_some());
    }

    #[test]
    fn params_deserialize_json_object_body() {
        let json = r#"{
            "url": "https://example.com",
            "method": "POST",
            "body": {"q": 1, "limit": 2}
        }"#;
        let params: Params = serde_json::from_str(json).unwrap();
        let body = params.body.unwrap().into_string().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            serde_json::json!({"q": 1, "limit": 2})
        );
    }

    #[test]
    fn params_deserialize_json_array_body() {
        let json = r#"{
            "url": "https://example.com",
            "method": "POST",
            "body": ["a", "b"]
        }"#;
        let params: Params = serde_json::from_str(json).unwrap();
        let body = params.body.unwrap().into_string().unwrap();
        assert_eq!(body, r#"["a","b"]"#);
    }

    #[test]
    fn prepare_headers_injects_accept() {
        let headers = prepare_headers(&None, false);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Accept");
        assert_eq!(headers[0].1, "application/json");
    }

    #[test]
    fn prepare_headers_injects_content_type_with_body() {
        let headers = prepare_headers(&None, true);
        assert_eq!(headers.len(), 2);
        assert!(headers.iter().any(|(k, _)| k == "Accept"));
        assert!(headers.iter().any(|(k, _)| k == "Content-Type"));
    }

    #[test]
    fn prepare_headers_no_content_type_without_body() {
        let headers = prepare_headers(&None, false);
        assert!(!headers.iter().any(|(k, _)| k == "Content-Type"));
    }

    #[test]
    fn prepare_headers_preserves_user_accept() {
        let mut user = std::collections::HashMap::new();
        user.insert("Accept".to_string(), "text/xml".to_string());
        let headers = prepare_headers(&Some(user), false);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].1, "text/xml");
    }

    #[test]
    fn prepare_headers_preserves_user_content_type() {
        let mut user = std::collections::HashMap::new();
        user.insert("content-type".to_string(), "text/plain".to_string());
        let headers = prepare_headers(&Some(user), true);
        // Should have user's content-type + auto Accept, but NOT auto Content-Type
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "content-type" && v == "text/plain")
        );
        assert!(
            !headers
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "application/json")
        );
    }

    #[test]
    fn prepare_headers_case_insensitive_check() {
        let mut user = std::collections::HashMap::new();
        user.insert("ACCEPT".to_string(), "text/html".to_string());
        let headers = prepare_headers(&Some(user), false);
        // Should not add a second Accept
        let accept_count = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("accept"))
            .count();
        assert_eq!(accept_count, 1);
    }

    #[test]
    fn do_paid_fetch_returns_error_for_invalid_url() {
        let result = do_paid_fetch("GET", "not-a-url", &[], None, None);
        assert!(result.is_err());
    }

    #[test]
    fn request_validation_errors_are_returned_as_tool_content() {
        let result = pay_error_to_tool_result(pay_core::Error::RequestValidation(
            "body.email is required".to_string(),
        ));

        assert_eq!(result.is_error, Some(true));
        let text = result.content[0].as_text().unwrap();
        assert_eq!(text.text, "body.email is required");
    }

    #[test]
    fn payment_errors_are_returned_as_tool_content() {
        let result = pay_error_to_tool_result(pay_core::Error::PaymentRejected(
            "insufficient funds".to_string(),
        ));

        assert_eq!(result.is_error, Some(true));
        let text = result.content[0].as_text().unwrap();
        assert_eq!(
            text.text,
            "Pay curl failed: Payment rejected: insufficient funds"
        );
    }

    #[test]
    fn user_rejection_emits_clarification_guidance_macos() {
        let result = pay_error_to_tool_result(pay_core::Error::PaymentRejected(
            "rejected by user at Apple Keychain".to_string(),
        ));

        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains("User declined"));
        assert!(text.contains("Apple Keychain"));
        assert!(text.contains("NOT sent"));
        assert!(text.contains("clarification"));
    }

    #[test]
    fn user_rejection_emits_clarification_guidance_windows() {
        let result = pay_error_to_tool_result(pay_core::Error::PaymentRejected(
            "rejected by user at Windows Hello".to_string(),
        ));
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains("User declined"));
        assert!(text.contains("Windows Hello"));
    }

    #[test]
    fn user_rejection_emits_clarification_guidance_linux() {
        let result = pay_error_to_tool_result(pay_core::Error::PaymentRejected(
            "rejected by user at GNOME Keyring".to_string(),
        ));
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains("User declined"));
        assert!(text.contains("GNOME Keyring"));
    }

    #[test]
    fn server_rejection_does_not_use_user_rejection_path() {
        // Server-side verification_failed → must keep the original "Pay curl
        // failed" prefix so the LLM sees it as a server error, not a user
        // declination.
        let result = pay_error_to_tool_result(pay_core::Error::PaymentRejected(
            "wrong network: expected localnet".to_string(),
        ));
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.starts_with("Pay curl failed: Payment rejected:"));
        assert!(!text.contains("User declined"));
    }

    // ── Env var propagation for network/account overrides ─────────────

    #[test]
    fn network_override_reads_from_env() {
        // Simulate what main.rs sets when --sandbox is used
        unsafe { std::env::set_var("PAY_NETWORK_ENFORCED", "localnet") };
        let val = std::env::var("PAY_NETWORK_ENFORCED").ok();
        assert_eq!(val.as_deref(), Some("localnet"));
        unsafe { std::env::remove_var("PAY_NETWORK_ENFORCED") };

        // Without the env var, returns None
        let val = std::env::var("PAY_NETWORK_ENFORCED").ok();
        assert!(val.is_none());
    }

    #[test]
    fn account_override_reads_from_env() {
        unsafe { std::env::set_var("PAY_ACTIVE_ACCOUNT", "my-wallet") };
        let val = std::env::var("PAY_ACTIVE_ACCOUNT").ok();
        assert_eq!(val.as_deref(), Some("my-wallet"));
        unsafe { std::env::remove_var("PAY_ACTIVE_ACCOUNT") };
    }

    #[test]
    fn x402_paid_fetch_supports_v1_and_v2_header_names() {
        assert_eq!(pay_core::x402::X402_V1_PAYMENT_HEADER, "X-PAYMENT");
        assert_eq!(pay_core::x402::X402_V2_PAYMENT_HEADER, "PAYMENT-SIGNATURE");
        assert_eq!(pay_core::x402::SIGN_IN_WITH_X_HEADER, "SIGN-IN-WITH-X");
    }

    // ── body_to_mcp_content content-type routing ──────────────────────
    //
    // Regression coverage for #350.4: pay-mcp must keep binary payloads
    // intact across the MCP transport. Text → Content::text, image →
    // base64 Content::image, other binary → tempfile path.

    #[test]
    fn is_binary_content_type_recognizes_text() {
        assert!(!is_binary_content_type("text/plain"));
        assert!(!is_binary_content_type("text/html"));
        assert!(!is_binary_content_type("text/csv"));
        assert!(!is_binary_content_type("application/json"));
        assert!(!is_binary_content_type("application/xml"));
        assert!(!is_binary_content_type("application/ld+json"));
        assert!(!is_binary_content_type("application/atom+xml"));
    }

    #[test]
    fn is_binary_content_type_recognizes_binary() {
        assert!(is_binary_content_type("application/pdf"));
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(is_binary_content_type("application/zip"));
        assert!(is_binary_content_type("image/png"));
        assert!(is_binary_content_type("audio/mpeg"));
        assert!(is_binary_content_type("video/mp4"));
    }

    #[test]
    fn body_to_mcp_content_routes_text_as_text() {
        let body = b"plain string".to_vec();
        let content = body_to_mcp_content(body, Some("text/plain"), "empty");
        assert_eq!(content.len(), 1);
        let text = content[0].as_text().expect("text content").text.clone();
        assert_eq!(text, "plain string");
    }

    #[test]
    fn body_to_mcp_content_routes_json_as_text() {
        let body = br#"{"ok":true}"#.to_vec();
        let content = body_to_mcp_content(body, Some("application/json"), "empty");
        let text = content[0].as_text().expect("text content").text.clone();
        assert_eq!(text, r#"{"ok":true}"#);
    }

    #[test]
    fn body_to_mcp_content_strips_charset_parameter() {
        let body = b"hello".to_vec();
        let content = body_to_mcp_content(body, Some("text/plain; charset=utf-8"), "empty");
        let text = content[0].as_text().expect("text content").text.clone();
        assert_eq!(text, "hello");
    }

    #[test]
    fn body_to_mcp_content_routes_image_as_base64_image() {
        // Real PNG signature so encoding is meaningful.
        let body: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let content = body_to_mcp_content(body.clone(), Some("image/png"), "empty");
        assert_eq!(content.len(), 1);
        let image = content[0].as_image().expect("image content");
        assert_eq!(image.mime_type, "image/png");
        let decoded = general_purpose::STANDARD.decode(&image.data).unwrap();
        assert_eq!(decoded, body, "base64 round-trips byte-for-byte");
    }

    #[test]
    fn body_to_mcp_content_spills_pdf_to_tempfile() {
        let body: Vec<u8> = b"%PDF-1.4 fake content with \xFF\xFE bytes".to_vec();
        let content = body_to_mcp_content(body.clone(), Some("application/pdf"), "empty");
        let text = content[0].as_text().expect("text content").text.clone();
        // Text content should describe the spill and contain a path
        assert!(text.contains("Binary response"));
        assert!(text.contains("application/pdf"));
        // Extract the path and verify the file contents match exactly
        let path = text.split(" written to ").nth(1).expect("path in message");
        let on_disk = std::fs::read(path).expect("tempfile readable");
        assert_eq!(on_disk, body, "spilled bytes preserved");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn body_to_mcp_content_octet_stream_spills_to_tempfile() {
        let body: Vec<u8> = vec![0x00, 0x01, 0x02, 0xFF, 0xFE, 0xFD];
        let content = body_to_mcp_content(body.clone(), Some("application/octet-stream"), "empty");
        let text = content[0].as_text().expect("text content").text.clone();
        let path = text.split(" written to ").nth(1).expect("path in message");
        let on_disk = std::fs::read(path).expect("tempfile readable");
        assert_eq!(on_disk, body);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn body_to_mcp_content_empty_body_returns_message() {
        let content = body_to_mcp_content(vec![], Some("application/json"), "Request completed.");
        let text = content[0].as_text().expect("text content").text.clone();
        assert_eq!(text, "Request completed.");
    }

    #[test]
    fn body_to_mcp_content_missing_content_type_treats_as_binary() {
        // No content-type → treat as octet-stream (safer than mangling
        // potential binary payload through UTF-8 lossy decode).
        let body: Vec<u8> = vec![0xFF, 0xFE, 0x00];
        let content = body_to_mcp_content(body.clone(), None, "empty");
        let text = content[0].as_text().expect("text content").text.clone();
        assert!(text.contains("Binary response"));
        let path = text.split(" written to ").nth(1).expect("path in message");
        let on_disk = std::fs::read(path).expect("tempfile readable");
        assert_eq!(on_disk, body);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn body_to_mcp_content_text_with_invalid_utf8_uses_replacement_chars() {
        // Caller advertised text/plain but body has invalid UTF-8 — we keep
        // it as text and replace bad sequences (data is lost, but caller
        // chose the text route by labeling it text/plain).
        let body: Vec<u8> = vec![b'h', b'i', 0xFF, 0xFE];
        let content = body_to_mcp_content(body, Some("text/plain"), "empty");
        let text = content[0].as_text().expect("text content").text.clone();
        assert!(text.starts_with("hi"));
        assert!(text.contains('\u{FFFD}'));
    }

    #[test]
    fn extension_for_mime_known_types() {
        assert_eq!(extension_for_mime("application/pdf"), ".pdf");
        assert_eq!(extension_for_mime("image/png"), ".png");
        // mime_guess returns the first registered extension, which is
        // database-version dependent (e.g. JPEG resolves to ".jpe" today).
        // Just assert we get a non-empty leading-dot extension that's
        // not the generic fallback.
        let jpg = extension_for_mime("image/jpeg");
        assert!(jpg.starts_with('.'));
        assert_ne!(jpg, ".bin");
    }

    #[test]
    fn extension_for_mime_unknown_falls_back_to_bin() {
        assert_eq!(extension_for_mime("application/x-totally-made-up"), ".bin");
        assert_eq!(extension_for_mime(""), ".bin");
    }
}
