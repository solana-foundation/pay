//! Shared validation and opaque-token helpers for browser authorization flows.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Url;

/// JSON error envelope returned by pay-connect APIs.
#[derive(Debug, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: StatusCode,
    pub error: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, error: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            error,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn bad_request(error: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error, message)
    }

    pub fn invalid_grant() -> Self {
        Self::bad_request(
            "invalid_grant",
            "The authorization code is unknown, expired, already used, or the PKCE verifier does not match.",
        )
    }

    pub fn busy() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "Too many sign-ins are in progress. Try again in a few minutes.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

/// Restrict CLI callbacks to an exact loopback path with no attached data.
pub fn validate_callback(callback: &str) -> Result<Url, ApiError> {
    let err = |message: &str| ApiError::bad_request("invalid_callback", message);
    let url =
        Url::parse(callback).map_err(|error| err(&format!("callback is not a URL: {error}")))?;
    if url.scheme() != "http" {
        return Err(err("callback must use http"));
    }
    if !matches!(url.host_str(), Some("127.0.0.1") | Some("localhost")) {
        return Err(err("callback host must be 127.0.0.1 or localhost"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(err("callback must not carry credentials"));
    }
    if url.path() != "/callback" {
        return Err(err("callback path must be /callback"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(err("callback must not have a query string or fragment"));
    }
    Ok(url)
}

pub fn is_base64url_alphabet(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn validate_token(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len()) && is_base64url_alphabet(value)
}

pub fn validate_state(state: &str) -> Result<(), ApiError> {
    if validate_token(state, 16, 128) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "invalid_state",
            "state must be 16 to 128 base64url characters",
        ))
    }
}

pub fn validate_code_challenge(challenge: &str) -> Result<(), ApiError> {
    if validate_token(challenge, 43, 128) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "invalid_code_challenge",
            "code_challenge must be 43 to 128 base64url characters",
        ))
    }
}

pub fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(input: &str) -> String {
    Sha256::digest(input.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn redirect_url(callback: &str, code: &str, state: &str) -> String {
    let mut url = Url::parse(callback).expect("callback validated before request creation");
    url.query_pairs_mut()
        .append_pair("code", code)
        .append_pair("state", state);
    url.into()
}
