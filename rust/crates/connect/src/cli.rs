//! Privy-backed browser linking for the `pay` CLI.
//!
//! `GET /v1/cli` validates the loopback callback and PKCE challenge, stores a
//! short-lived request, and sends the browser to the shared pay.sh connect
//! page. Approval binds the same Privy tenant used by MCP and returns a
//! one-time code to the loopback listener. `POST /v1/cli/complete` consumes
//! that code and returns only a tenant-scoped pay-connect token; Privy's app
//! credentials never leave this service.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::oauth::{PrivyLogin, approve_with_privy, privy_login};
use crate::protocol::{
    ApiError, pkce_challenge, random_token, redirect_url, sha256_hex, validate_callback,
    validate_code_challenge, validate_state,
};

const REQUEST_TTL: Duration = Duration::from_secs(10 * 60);
const CODE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_ROWS: usize = 4096;

#[derive(Clone)]
struct Pending {
    callback: String,
    state: String,
    code_challenge: String,
    created_at: Instant,
}

struct Grant {
    subject: String,
    code_challenge: String,
    created_at: Instant,
}

/// In-memory v0 store. Only hashes of exchange codes and API tokens are kept.
#[derive(Default)]
pub struct Store {
    pending: Mutex<HashMap<String, Pending>>,
    grants: Mutex<HashMap<String, Grant>>,
    tokens: Mutex<HashMap<String, String>>,
}

impl Store {
    fn insert_pending(&self, id: String, pending: Pending) -> Result<(), ApiError> {
        let now = Instant::now();
        let mut rows = self.pending.lock().unwrap();
        rows.retain(|_, row| now.saturating_duration_since(row.created_at) <= REQUEST_TTL);
        if rows.len() >= MAX_ROWS {
            return Err(ApiError::busy());
        }
        rows.insert(id, pending);
        Ok(())
    }

    fn pending(&self, id: &str) -> Option<Pending> {
        let row = self.pending.lock().unwrap().get(id).cloned()?;
        (Instant::now().saturating_duration_since(row.created_at) <= REQUEST_TTL).then_some(row)
    }

    fn approve(&self, id: &str, subject: String) -> Option<(Pending, String)> {
        let pending = self.pending.lock().unwrap().remove(id)?;
        if Instant::now().saturating_duration_since(pending.created_at) > REQUEST_TTL {
            return None;
        }
        let code = random_token();
        let mut grants = self.grants.lock().unwrap();
        if grants.len() >= MAX_ROWS {
            let now = Instant::now();
            grants.retain(|_, row| now.saturating_duration_since(row.created_at) <= CODE_TTL);
            if grants.len() >= MAX_ROWS {
                return None;
            }
        }
        grants.insert(
            sha256_hex(&code),
            Grant {
                subject,
                code_challenge: pending.code_challenge.clone(),
                created_at: Instant::now(),
            },
        );
        Some((pending, code))
    }

    fn deny(&self, id: &str) -> Option<Pending> {
        let pending = self.pending.lock().unwrap().remove(id)?;
        (Instant::now().saturating_duration_since(pending.created_at) <= REQUEST_TTL)
            .then_some(pending)
    }

    fn exchange(&self, code: &str, verifier: &str) -> Option<String> {
        let grant = self.grants.lock().unwrap().remove(&sha256_hex(code))?;
        if Instant::now().saturating_duration_since(grant.created_at) > CODE_TTL
            || pkce_challenge(verifier) != grant.code_challenge
        {
            return None;
        }
        Some(grant.subject)
    }

    fn issue_token(&self, subject: String) -> Result<String, ApiError> {
        let token = format!("pct_{}", random_token());
        let mut tokens = self.tokens.lock().unwrap();
        if tokens.len() >= MAX_ROWS {
            return Err(ApiError::busy());
        }
        tokens.insert(sha256_hex(&token), subject);
        Ok(token)
    }

    pub fn authenticate(&self, token: &str) -> Option<String> {
        self.tokens.lock().unwrap().get(&sha256_hex(token)).cloned()
    }
}

#[derive(Deserialize)]
pub struct StartQuery {
    callback: String,
    state: String,
    code_challenge: String,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    cli: Option<String>,
}

/// Validate and reserve a CLI link, then enter the existing `/connect` UI.
pub async fn start(
    State(state): State<AppState>,
    Query(query): Query<StartQuery>,
) -> Result<Redirect, ApiError> {
    validate_callback(&query.callback)?;
    validate_state(&query.state)?;
    validate_code_challenge(&query.code_challenge)?;
    let id = random_token();
    state.cli().insert_pending(
        id.clone(),
        Pending {
            callback: query.callback,
            state: query.state,
            code_challenge: query.code_challenge,
            created_at: Instant::now(),
        },
    )?;
    tracing::info!(
        account = query.account.as_deref().unwrap_or("-"),
        host = query.host.as_deref().unwrap_or("-"),
        cli = query.cli.as_deref().unwrap_or("-"),
        "CLI wallet link started"
    );
    Ok(Redirect::to(&state.cli_page_url(&id)))
}

#[derive(Serialize)]
pub struct PendingView {
    pub client_name: &'static str,
    pub scope: &'static str,
    pub has_wallet: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privy: Option<PrivyLogin>,
}

pub async fn pending_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<PendingView>, ApiError> {
    state.cli().pending(&id).ok_or_else(unknown_request)?;
    let wallet = state
        .tenants()
        .subject_from_cookie(&headers)
        .and_then(|subject| state.tenants().get(&subject));
    Ok(Json(PendingView {
        client_name: "pay CLI",
        scope: "cli",
        has_wallet: wallet.is_some(),
        wallet_address: wallet.map(|wallet| wallet.pubkey.clone()),
        privy: privy_login(&state),
    }))
}

#[derive(Serialize)]
pub struct Decision {
    pub redirect: String,
}

pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (subject, fresh_login) = match bearer {
        Some(token) => (approve_with_privy(&state, token).await?.0, true),
        None => (
            state
                .tenants()
                .subject_from_cookie(&headers)
                .filter(|subject| state.tenants().get(subject).is_some())
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "no_wallet",
                        "This browser has no pay wallet yet. Sign in first.",
                    )
                })?,
            false,
        ),
    };
    let (pending, code) = state
        .cli()
        .approve(&id, subject.clone())
        .ok_or_else(unknown_request)?;
    let mut response = Json(Decision {
        redirect: redirect_url(&pending.callback, &code, &pending.state),
    })
    .into_response();
    if fresh_login {
        response.headers_mut().insert(
            header::SET_COOKIE,
            state
                .tenants()
                .subject_cookie(&subject, state.public_url().starts_with("https://")),
        );
    }
    Ok(response)
}

pub async fn deny(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Decision>, ApiError> {
    let pending = state.cli().deny(&id).ok_or_else(unknown_request)?;
    let mut callback = url::Url::parse(&pending.callback).expect("validated CLI callback");
    callback
        .query_pairs_mut()
        .append_pair("error", "access_denied")
        .append_pair("state", &pending.state);
    Ok(Json(Decision {
        redirect: callback.into(),
    }))
}

#[derive(Deserialize)]
pub struct CompleteRequest {
    code: String,
    code_verifier: String,
}

#[derive(Serialize)]
pub struct CompleteResponse {
    provider: &'static str,
    status: &'static str,
    network: &'static str,
    wallet_id: String,
    pubkey: String,
    credentials: std::collections::BTreeMap<String, String>,
}

pub async fn complete(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<CompleteResponse>, ApiError> {
    let request: CompleteRequest = serde_json::from_slice(&body).map_err(|error| {
        ApiError::bad_request("invalid_request", format!("invalid JSON body: {error}"))
    })?;
    let subject = state
        .cli()
        .exchange(&request.code, &request.code_verifier)
        .ok_or_else(ApiError::invalid_grant)?;
    let tenant = state
        .tenants()
        .get(&subject)
        .ok_or_else(ApiError::invalid_grant)?;
    let token = state.cli().issue_token(subject)?;
    Ok(Json(CompleteResponse {
        provider: "payconnect",
        status: "ready",
        network: "mainnet",
        wallet_id: tenant.wallet_id.clone(),
        pubkey: tenant.pubkey.clone(),
        credentials: [("api_token".to_string(), token)].into_iter().collect(),
    }))
}

fn unknown_request() -> ApiError {
    ApiError::bad_request(
        "unknown_request",
        "This CLI link is unknown or expired. Run `pay setup` again.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use pay_mcp::policy::SpendPolicy;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const CALLBACK: &str = "http://127.0.0.1:53211/callback";
    const STATE: &str = "abcdefghijklmnopqrstuvwxyz012345";
    const VERIFIER: &str = "test-verifier-not-a-secret-00000000000000000000";
    const SUBJECT: &str = "sub_cli_test";
    const WALLET: &str = "wallet_cli_test";
    const ADDRESS: &str = "CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z";

    fn state() -> AppState {
        let state = AppState::new("https://connect.test").with_pages_url("https://pages.test");
        let credentials = [
            (
                "app_id".to_string(),
                "test_app_id_not_a_real_credential".to_string(),
            ),
            (
                "app_secret".to_string(),
                "test_app_secret_not_a_real_credential".to_string(),
            ),
            (
                "authorization_key".to_string(),
                "test_authorization_key_not_a_real_credential".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        state.tenants().bind(crate::tenants::TenantRecord {
            subject: SUBJECT.to_string(),
            account_name: crate::tenants::CONNECTOR_ACCOUNT.to_string(),
            provider: "privy".to_string(),
            wallet_id: WALLET.to_string(),
            pubkey: ADDRESS.to_string(),
            credentials,
            policy: SpendPolicy::dollars(1.0, 10.0),
        });
        state
    }

    async fn json(response: axum::response::Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn browser_cookie_links_cli_and_issues_tenant_scoped_token() {
        let state = state();
        let app = crate::router(state.clone());
        let challenge = pkce_challenge(VERIFIER);
        let start_uri = format!(
            "/v1/cli?callback=http%3A%2F%2F127.0.0.1%3A53211%2Fcallback&state={STATE}&code_challenge={challenge}"
        );
        let start = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(start_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(start.status(), StatusCode::SEE_OTHER);
        let page = url::Url::parse(start.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        assert_eq!(page.origin().ascii_serialization(), "https://pages.test");
        assert_eq!(page.path(), "/connect");
        let request_id = page
            .query_pairs()
            .find(|(key, _)| key == "cli")
            .map(|(_, value)| value.into_owned())
            .unwrap();

        let set_cookie = state.tenants().subject_cookie(SUBJECT, true);
        let cookie = set_cookie.to_str().unwrap().split(';').next().unwrap();
        let view = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/cli/{request_id}"))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(view).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["client_name"], "pay CLI");
        assert_eq!(body["has_wallet"], true);
        assert_eq!(body["wallet_address"], ADDRESS);

        let approval = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/cli/{request_id}/approve"))
                    .header(header::COOKIE, cookie)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(approval).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let callback = url::Url::parse(body["redirect"].as_str().unwrap()).unwrap();
        assert_eq!(callback.as_str().split('?').next(), Some(CALLBACK));
        let query: HashMap<_, _> = callback.query_pairs().into_owned().collect();
        assert_eq!(query["state"], STATE);
        let code = &query["code"];

        let complete = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/cli/complete")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "code": code,
                            "code_verifier": VERIFIER,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(complete).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["provider"], "payconnect");
        assert_eq!(body["wallet_id"], WALLET);
        assert_eq!(body["pubkey"], ADDRESS);
        let token = body["credentials"]["api_token"].as_str().unwrap();
        assert!(token.starts_with("pct_"));

        let wallets = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/wallets")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(wallets).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!([{ "id": WALLET, "address": ADDRESS }]));

        let replay = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/cli/complete")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "code": code,
                            "code_verifier": VERIFIER,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(replay).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_grant");
    }
}
