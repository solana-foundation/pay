use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

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
    #[schemars(description = "Request body string (for POST, PUT, etc.)")]
    pub body: Option<String>,
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
    let body = params.body.clone();
    let url = params.url.clone();

    let response =
        tokio::task::spawn_blocking(move || do_paid_fetch(&method, &url, &headers, body))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        response,
    )]))
}

fn do_paid_fetch(
    method: &str,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<String>,
) -> Result<String, pay_core::Error> {
    use pay_core::client::runner::RunOutcome;

    let outcome =
        pay_core::client::fetch::fetch_request(method, url, extra_headers, body.as_deref())?;
    let store = pay_core::accounts::FileAccountsStore::default_path();

    match outcome {
        RunOutcome::MppChallenge { challenge, .. } => {
            let (auth_header, _ephemeral) =
                pay_core::client::mpp::build_credential(&challenge, &store, None, None)?;
            let mut headers = extra_headers.to_vec();
            headers.push(("Authorization".to_string(), auth_header));
            interpret_retry(pay_core::client::fetch::fetch_request(
                method,
                url,
                &headers,
                body.as_deref(),
            )?)
        }
        RunOutcome::X402Challenge { requirements, .. } => {
            let (payment_header, _ephemeral) =
                pay_core::client::x402::build_payment(&requirements, &store, None, None)?;
            let mut headers = extra_headers.to_vec();
            headers.push(("X-PAYMENT".to_string(), payment_header));
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
}
