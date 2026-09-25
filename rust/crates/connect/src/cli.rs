//! Privy-backed browser linking for the `pay` CLI.
//!
//! `GET /v1/cli` validates the loopback callback and PKCE challenge, stores a
//! short-lived request, and sends the browser to the shared pay.sh connect
//! page. Approval binds the same Privy tenant used by MCP and returns a
//! one-time code to the loopback listener. `POST /v1/cli/complete` consumes
//! that code and returns only a tenant-scoped pay-connect token; Privy's app
//! credentials never leave this service.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use crate::AppState;
use crate::oauth::{FundNext, PrivyLogin, approve_with_privy, privy_login};
use crate::protocol::{
    ApiError, pkce_challenge, random_token, redirect_url, sha256_hex, validate_callback,
    validate_code_challenge, validate_state,
};

const REQUEST_TTL: Duration = Duration::from_secs(10 * 60);
const CODE_TTL: Duration = Duration::from_secs(5 * 60);
const TOKEN_TTL: Duration = Duration::from_secs(90 * 24 * 60 * 60);
const MAX_ROWS: usize = 4096;

#[derive(Clone)]
struct Pending {
    callback: String,
    state: String,
    kind: PendingKind,
    created_at: Instant,
}

#[derive(Clone)]
enum PendingKind {
    Link {
        code_challenge: String,
    },
    Topup {
        address: String,
        payment_method: Option<String>,
    },
}

struct Grant {
    subject: String,
    code_challenge: String,
    created_at: Instant,
}

struct Token {
    subject: String,
    created_at: Instant,
}

/// In-memory v0 store. Only hashes of exchange codes and API tokens are kept.
#[derive(Default)]
pub struct Store {
    pending: Mutex<HashMap<String, Pending>>,
    grants: Mutex<HashMap<String, Grant>>,
    tokens: Mutex<HashMap<String, Token>>,
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
        let mut rows = self.pending.lock().unwrap();
        let pending = rows.get(id)?;
        let PendingKind::Link { code_challenge } = &pending.kind else {
            return None;
        };
        let code_challenge = code_challenge.clone();
        let pending = rows.remove(id)?;
        drop(rows);
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
                code_challenge,
                created_at: Instant::now(),
            },
        );
        Some((pending, code))
    }

    fn deny(&self, id: &str) -> Option<Pending> {
        let mut rows = self.pending.lock().unwrap();
        if !matches!(rows.get(id)?.kind, PendingKind::Link { .. }) {
            return None;
        }
        let pending = rows.remove(id)?;
        (Instant::now().saturating_duration_since(pending.created_at) <= REQUEST_TTL)
            .then_some(pending)
    }

    fn complete_topup(&self, id: &str) -> Option<Pending> {
        let mut rows = self.pending.lock().unwrap();
        if !matches!(rows.get(id)?.kind, PendingKind::Topup { .. }) {
            return None;
        }
        let pending = rows.remove(id)?;
        (Instant::now().saturating_duration_since(pending.created_at) <= REQUEST_TTL)
            .then_some(pending)
    }

    fn is_link_pending(&self, id: &str) -> bool {
        self.pending(id)
            .is_some_and(|pending| matches!(pending.kind, PendingKind::Link { .. }))
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
        let now = Instant::now();
        tokens.retain(|_, row| now.saturating_duration_since(row.created_at) <= TOKEN_TTL);
        if tokens.len() >= MAX_ROWS {
            let oldest = tokens
                .iter()
                .min_by_key(|(_, row)| row.created_at)
                .map(|(hash, _)| hash.clone())
                .expect("a full token store has an oldest row");
            tokens.remove(&oldest);
        }
        tokens.insert(
            sha256_hex(&token),
            Token {
                subject,
                created_at: now,
            },
        );
        Ok(token)
    }

    pub fn authenticate(&self, token: &str) -> Option<String> {
        let hash = sha256_hex(token);
        let mut tokens = self.tokens.lock().unwrap();
        let row = tokens.get(&hash)?;
        if Instant::now().saturating_duration_since(row.created_at) > TOKEN_TTL {
            tokens.remove(&hash);
            return None;
        }
        Some(row.subject.clone())
    }

    fn revoke(&self, token: &str) -> bool {
        self.tokens
            .lock()
            .unwrap()
            .remove(&sha256_hex(token))
            .is_some()
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
            kind: PendingKind::Link {
                code_challenge: query.code_challenge,
            },
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

#[derive(Deserialize)]
pub struct TopupQuery {
    address: String,
    callback: String,
    state: String,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    cli: Option<String>,
    #[serde(default)]
    method: Option<String>,
}

/// Reserve a direct CLI top-up and enter `/connect` without wallet sign-in.
pub async fn start_topup(
    State(state): State<AppState>,
    Query(query): Query<TopupQuery>,
) -> Result<Redirect, ApiError> {
    validate_callback(&query.callback)?;
    validate_state(&query.state)?;
    let address = Pubkey::from_str(&query.address)
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_address",
                "A valid Solana wallet address is required.",
            )
        })?
        .to_string();
    let payment_method = query
        .method
        .filter(|method| matches!(method.as_str(), "card" | "apple-pay" | "google-pay"));
    let id = random_token();
    state.cli().insert_pending(
        id.clone(),
        Pending {
            callback: query.callback,
            state: query.state,
            kind: PendingKind::Topup {
                address: address.clone(),
                payment_method,
            },
            created_at: Instant::now(),
        },
    )?;
    tracing::info!(
        account = query.account.as_deref().unwrap_or("-"),
        cli = query.cli.as_deref().unwrap_or("-"),
        %address,
        "CLI top-up started"
    );
    Ok(Redirect::to(&state.cli_page_url(&id)))
}

#[derive(Serialize)]
pub struct PendingView {
    pub client_name: &'static str,
    pub scope: &'static str,
    pub intent: &'static str,
    pub has_wallet: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privy: Option<PrivyLogin>,
}

pub async fn pending_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<PendingView>, ApiError> {
    let pending = state.cli().pending(&id).ok_or_else(unknown_request)?;
    if let PendingKind::Topup {
        address,
        payment_method,
    } = pending.kind
    {
        return Ok(Json(PendingView {
            client_name: "pay CLI",
            scope: "cli",
            intent: "topup",
            has_wallet: true,
            wallet_address: Some(address),
            payment_method,
            privy: None,
        }));
    }
    let wallet = state
        .tenants()
        .subject_from_cookie(&headers)
        .and_then(|subject| state.tenants().get(&subject));
    Ok(Json(PendingView {
        client_name: "pay CLI",
        scope: "cli",
        intent: "link",
        has_wallet: wallet.is_some(),
        wallet_address: wallet.map(|wallet| wallet.pubkey.clone()),
        payment_method: None,
        privy: privy_login(&state),
    }))
}

#[derive(Serialize)]
pub struct Decision {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fund: Option<FundNext>,
}

pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !state.cli().is_link_pending(&id) {
        return Err(unknown_request());
    }
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
    if let Some(fund) = funding_next(&state, &id, &subject, fresh_login).await? {
        tracing::info!(subject = %subject, address = %fund.address, "empty CLI wallet: funding before approval");
        let mut response = Json(Decision {
            redirect: None,
            fund: Some(fund),
        })
        .into_response();
        response.headers_mut().insert(
            header::SET_COOKIE,
            state
                .tenants()
                .subject_cookie(&subject, state.public_url().starts_with("https://")),
        );
        return Ok(response);
    }
    let (pending, code) = state
        .cli()
        .approve(&id, subject.clone())
        .ok_or_else(unknown_request)?;
    let mut response = Json(Decision {
        redirect: Some(redirect_url(&pending.callback, &code, &pending.state)),
        fund: None,
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

#[derive(Deserialize)]
pub struct TopupCompleteRequest {
    payment_id: String,
    #[serde(default)]
    signature: Option<String>,
}

/// Return a settled direct top-up to the CLI loopback listener.
pub async fn complete_topup(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Decision>, ApiError> {
    let request: TopupCompleteRequest = serde_json::from_slice(&body).map_err(|error| {
        ApiError::bad_request("invalid_request", format!("invalid JSON body: {error}"))
    })?;
    if request.payment_id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "invalid_request",
            "A valid payment ID is required.",
        ));
    }
    let pending = state.cli().pending(&id).ok_or_else(unknown_request)?;
    let PendingKind::Topup { address, .. } = &pending.kind else {
        return Err(unknown_request());
    };
    if !state.wallet_probe().has_funds(address).await {
        return Err(ApiError::bad_request(
            "payment_pending",
            "Funding has not settled for this account yet.",
        ));
    }
    let pending = state
        .cli()
        .complete_topup(&id)
        .ok_or_else(unknown_request)?;
    let mut callback = url::Url::parse(&pending.callback).expect("validated CLI callback");
    let mut query = callback.query_pairs_mut();
    query
        .append_pair("payment_id", request.payment_id.trim())
        .append_pair("state", &pending.state);
    if let Some(signature) = request
        .signature
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        query.append_pair("signature", signature);
    }
    drop(query);
    Ok(Json(Decision {
        redirect: Some(callback.into()),
        fund: None,
    }))
}

async fn funding_next(
    state: &AppState,
    id: &str,
    subject: &str,
    fresh_login: bool,
) -> Result<Option<FundNext>, ApiError> {
    if !fresh_login {
        return Ok(None);
    }
    let tenant = state.tenants().get(subject).ok_or_else(unknown_request)?;
    if !state.wallet_probe().holds_nothing(&tenant.pubkey).await {
        return Ok(None);
    }
    if !state.cli().is_link_pending(id) {
        return Err(unknown_request());
    }
    Ok(Some(FundNext {
        address: tenant.pubkey.clone(),
        request: id.to_string(),
    }))
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
        redirect: Some(callback.into()),
        fund: None,
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
    expires_in: u64,
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
        expires_in: TOKEN_TTL.as_secs(),
        credentials: [("api_token".to_string(), token)].into_iter().collect(),
    }))
}

/// Revoke the caller's CLI token. Revocation is idempotent from the client's
/// perspective, but an absent or malformed credential is still unauthorized.
pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let token = bearer_token(&headers).ok_or_else(invalid_token)?;
    if !state.cli().revoke(token) {
        return Err(invalid_token());
    }
    Ok(StatusCode::NO_CONTENT)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn invalid_token() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_token",
        "A valid CLI token is required.",
    )
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
        assert_eq!(body["intent"], "link");
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

        // A full live store must not turn successful onboarding into a
        // permanent 503. Completion evicts the oldest token below.
        {
            let now = Instant::now();
            let mut tokens = state.cli().tokens.lock().unwrap();
            for index in 0..MAX_ROWS {
                tokens.insert(
                    format!("existing-{index}"),
                    Token {
                        subject: format!("subject-{index}"),
                        created_at: now,
                    },
                );
            }
        }

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

        let revoked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/cli")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::NO_CONTENT);

        let after_revoke = app
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
        assert_eq!(after_revoke.status(), StatusCode::UNAUTHORIZED);

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

    #[tokio::test]
    async fn fresh_empty_wallet_keeps_cli_request_pending_for_funding() {
        let state = state().with_wallet_probe(std::sync::Arc::new(crate::FixedProbe(true)));
        state
            .cli()
            .insert_pending(
                "request-to-fund".to_string(),
                Pending {
                    callback: CALLBACK.to_string(),
                    state: STATE.to_string(),
                    kind: PendingKind::Link {
                        code_challenge: pkce_challenge(VERIFIER),
                    },
                    created_at: Instant::now(),
                },
            )
            .unwrap();

        let fund = funding_next(&state, "request-to-fund", SUBJECT, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fund.address, ADDRESS);
        assert_eq!(fund.request, "request-to-fund");
        assert!(state.cli().pending("request-to-fund").is_some());
        assert!(
            funding_next(&state, "request-to-fund", SUBJECT, false)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn unsettled_topup_cannot_be_completed_or_consumed() {
        let app =
            crate::router(state().with_wallet_probe(std::sync::Arc::new(crate::FixedProbe(true))));
        let start = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/cli?address={ADDRESS}&callback=http%3A%2F%2F127.0.0.1%3A53211%2Fcallback&state={STATE}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let page = url::Url::parse(start.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let request_id = page
            .query_pairs()
            .find(|(key, _)| key == "cli")
            .map(|(_, value)| value.into_owned())
            .unwrap();

        let completion = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/cli/{request_id}/topup"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"payment_id":"forged"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(completion).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "payment_pending");

        let still_pending = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/cli/{request_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(still_pending.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn direct_topup_bypasses_privy_and_returns_payment_to_cli() {
        let app =
            crate::router(state().with_wallet_probe(std::sync::Arc::new(crate::FixedProbe(false))));
        let start = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/cli?address={ADDRESS}&callback=http%3A%2F%2F127.0.0.1%3A53211%2Fcallback&state={STATE}&account=ludo&cli=0.29.0&method=google-pay"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(start.status(), StatusCode::SEE_OTHER);
        let page = url::Url::parse(start.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let request_id = page
            .query_pairs()
            .find(|(key, _)| key == "cli")
            .map(|(_, value)| value.into_owned())
            .unwrap();

        let view = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/cli/{request_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(view).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["intent"], "topup");
        assert_eq!(body["wallet_address"], ADDRESS);
        assert_eq!(body["payment_method"], "google-pay");
        assert!(body.get("privy").is_none());

        let complete = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/cli/{request_id}/topup"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"payment_id":"payment_123","signature":"sig_123"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(complete).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let callback = url::Url::parse(body["redirect"].as_str().unwrap()).unwrap();
        let query: HashMap<_, _> = callback.query_pairs().into_owned().collect();
        assert_eq!(query["payment_id"], "payment_123");
        assert_eq!(query["signature"], "sig_123");
        assert_eq!(query["state"], STATE);
    }

    #[test]
    fn expired_tokens_are_rejected_and_swept_before_issuing() {
        let store = Store::default();
        let expired_at = Instant::now() - TOKEN_TTL - Duration::from_secs(1);
        {
            let mut tokens = store.tokens.lock().unwrap();
            for index in 0..MAX_ROWS {
                tokens.insert(
                    format!("expired-{index}"),
                    Token {
                        subject: SUBJECT.to_string(),
                        created_at: expired_at,
                    },
                );
            }
            tokens.insert(
                sha256_hex("pct_expired"),
                Token {
                    subject: SUBJECT.to_string(),
                    created_at: expired_at,
                },
            );
        }

        assert_eq!(store.authenticate("pct_expired"), None);
        let fresh = store.issue_token(SUBJECT.to_string()).unwrap();
        assert_eq!(store.authenticate(&fresh).as_deref(), Some(SUBJECT));
        assert_eq!(store.tokens.lock().unwrap().len(), 1);
    }

    #[test]
    fn full_live_token_store_evicts_the_oldest_token() {
        let store = Store::default();
        let oldest = sha256_hex("pct_oldest");
        let now = Instant::now();
        {
            let mut tokens = store.tokens.lock().unwrap();
            tokens.insert(
                oldest.clone(),
                Token {
                    subject: "oldest".to_string(),
                    created_at: now - Duration::from_secs(1),
                },
            );
            for index in 1..MAX_ROWS {
                tokens.insert(
                    format!("live-{index}"),
                    Token {
                        subject: index.to_string(),
                        created_at: now,
                    },
                );
            }
        }

        let fresh = store.issue_token(SUBJECT.to_string()).unwrap();
        let tokens = store.tokens.lock().unwrap();
        assert_eq!(tokens.len(), MAX_ROWS);
        assert!(!tokens.contains_key(&oldest));
        assert!(tokens.contains_key(&sha256_hex(&fresh)));
    }
}
