use rmcp::model::CallToolResult;
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

pub async fn run(params: Params) -> Result<CallToolResult, rmcp::ErrorData> {
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

    let response =
        tokio::task::spawn_blocking(move || do_paid_fetch(&method, &url, &headers, body))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    match response {
        Ok(response) => Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            response,
        )])),
        Err(err) => Ok(pay_error_to_tool_result(err)),
    }
}

fn do_paid_fetch(
    method: &str,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<String>,
) -> Result<String, pay_core::Error> {
    use pay_core::client::runner::RunOutcome;

    pay_core::skills::validate_cached_catalog_request(method, url, body.as_deref())?;

    let outcome =
        pay_core::client::fetch::fetch_request(method, url, extra_headers, body.as_deref())?;
    let store = pay_core::accounts::FileAccountsStore::default_path();
    let network_override = std::env::var("PAY_NETWORK_ENFORCED").ok();
    let account_override = std::env::var("PAY_ACTIVE_ACCOUNT").ok();

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
            let (auth_header, _ephemeral) = pay_core::client::mpp::build_credential(
                selected,
                &store,
                network_override.as_deref(),
                account_override.as_deref(),
                Some(url),
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
            let built_payment = pay_core::client::x402::build_payment(
                &challenge,
                &store,
                network_override.as_deref(),
                account_override.as_deref(),
                Some(url),
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
        RunOutcome::X402SignInChallenge { challenge, .. } => {
            let built_payment = pay_core::client::x402::build_siwx_auth_header(
                &challenge,
                &store,
                network_override.as_deref(),
                account_override.as_deref(),
                Some(url),
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
        RunOutcome::SessionChallenge { .. } => Err(pay_core::Error::Mpp(
            "402 Payment Required (MPP session) — session payments require a stateful client with a Fiber channel".to_string(),
        )),
        RunOutcome::PaymentRejected { reason, .. } => Err(pay_core::Error::PaymentRejected(reason)),
        RunOutcome::UnknownPaymentRequired { .. } => Err(pay_core::Error::Mpp(
            "402 Payment Required but no recognized protocol".to_string(),
        )),
        RunOutcome::Completed { body, .. } => {
            Ok(body.unwrap_or_else(|| "Request completed.".to_string()))
        }
    }
}

fn pay_error_to_tool_result(err: pay_core::Error) -> CallToolResult {
    let message = match err {
        pay_core::Error::RequestValidation(message) => message,
        other => format!("Pay curl failed: {other}"),
    };
    super::tool_error(message)
}

fn interpret_retry(
    outcome: pay_core::client::runner::RunOutcome,
) -> Result<String, pay_core::Error> {
    use pay_core::client::runner::RunOutcome;
    match outcome {
        RunOutcome::Completed { body, .. } => {
            Ok(body.unwrap_or_else(|| "Payment successful.".to_string()))
        }
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
        let result = do_paid_fetch("GET", "not-a-url", &[], None);
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
}
