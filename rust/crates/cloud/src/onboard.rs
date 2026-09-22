//! Onboarding start/exchange: request validation, one-time codes, PKCE.
//!
//! Flow (RFC 7636 S256, loopback redirect like `gh auth login`):
//!
//! 1. The CLI opens `/onboard?callback=…&state=…&code_challenge=…`.
//! 2. The page posts the email + those params to `POST /api/onboard/start`.
//!    We mint a one-time `code`, store the session keyed by `sha256(code)`,
//!    and answer with `redirect = <callback>?code=…&state=…`.
//! 3. The browser follows the redirect to the CLI's loopback listener.
//! 4. The CLI posts `{ code, code_verifier }` to `POST /v1/onboard/exchange`;
//!    we verify `base64url(sha256(verifier)) == code_challenge`, consume the
//!    session, and return the (stub) onboarding result.

use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::AppState;
use crate::drivers::{DriverError, ProvisionedWallet};

/// How long a minted code stays exchangeable.
pub const SESSION_TTL: Duration = Duration::from_secs(5 * 60);

/// Network slug reported by the stub exchange response.
pub const NETWORK: &str = "mainnet";

/// A pending onboarding handshake, created by `/api/onboard/start`.
#[derive(Clone, Debug)]
pub struct OnboardSession {
    /// One-time code (32 random bytes, base64url, no padding). Only its
    /// SHA-256 is used as the store key.
    pub code: String,
    /// Email, when the page collected one (no-provider stub path).
    pub email: Option<String>,
    /// CLI CSRF state; also the `state` echoed by the provider's consent page.
    pub state: String,
    pub callback: String,
    pub code_challenge: String,
    /// Wallet driver chosen on the page (`openfort`), if any.
    pub provider: Option<String>,
    /// Filled by `/api/onboard/{provider}/complete` once the driver has run.
    pub wallet: Option<ProvisionedWallet>,
    /// A completion holds the session while its driver provisions; see
    /// [`AppState::claim_provisioning`].
    pub provisioning: bool,
    /// Connector origin: the pending OAuth authorization this wallet is
    /// for. The wallet then binds a tenant and approves that request
    /// instead of going back to a CLI; `callback` and `code_challenge` are
    /// empty.
    pub authorization: Option<String>,
    pub created_at: Instant,
}

impl OnboardSession {
    /// Mint a fresh session with a random code, timestamped now.
    pub fn new(
        email: Option<String>,
        state: String,
        callback: String,
        code_challenge: String,
        provider: Option<String>,
    ) -> Self {
        Self {
            code: random_token(),
            email,
            state,
            callback,
            code_challenge,
            provider,
            wallet: None,
            provisioning: false,
            authorization: None,
            created_at: Instant::now(),
        }
    }

    /// A session started from the OAuth consent page. The provider echoes
    /// the authorization request id as `state`.
    pub fn for_connector(authorization: &str, provider: &str) -> Self {
        Self {
            code: random_token(),
            email: None,
            state: authorization.to_string(),
            callback: String::new(),
            code_challenge: String::new(),
            provider: Some(provider.to_string()),
            wallet: None,
            provisioning: false,
            authorization: Some(authorization.to_string()),
            created_at: Instant::now(),
        }
    }

    pub fn is_expired_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created_at) > SESSION_TTL
    }

    /// Store key: hex-encoded SHA-256 of the code.
    pub fn key(&self) -> String {
        sha256_hex(&self.code)
    }

    /// Redirect back to the CLI: `<callback>?code=…&state=…`.
    pub fn redirect_url(&self) -> String {
        redirect_url(&self.callback, &self.code, &self.state)
    }
}

/// Body of `POST /api/onboard/start`.
#[derive(Debug, Deserialize)]
pub struct StartRequest {
    /// Required when no `provider` is given.
    #[serde(default)]
    pub email: Option<String>,
    /// Wallet driver id (`openfort`). When set, the response carries the
    /// provider's consent URL instead of a redirect.
    #[serde(default)]
    pub provider: Option<String>,
    /// CLI origin: the loopback callback, CSRF state and PKCE challenge.
    #[serde(default)]
    pub callback: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub code_challenge: Option<String>,
    /// Connector origin: the pending OAuth authorization to satisfy with
    /// the provisioned wallet. Requires `provider`.
    #[serde(default)]
    pub authorization_request: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub cli: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    /// Straight back to the CLI (no-provider stub path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<String>,
    /// Provider consent page to send the browser to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Body of `POST /api/onboard/{provider}/complete`: the URL fragment the
/// consent page redirected back with, verbatim.
#[derive(Debug, Deserialize)]
pub struct CompleteRequest {
    pub fragment: String,
}

#[derive(Debug, Serialize)]
pub struct CompleteResponse {
    /// `cli` when the browser goes back to a terminal, `connector` when it
    /// goes back to an MCP host.
    pub origin: &'static str,
    pub redirect: String,
    pub provider: String,
    pub address: String,
}

/// Body of `POST /v1/onboard/exchange`.
#[derive(Debug, Deserialize)]
pub struct ExchangeRequest {
    pub code: String,
    pub code_verifier: String,
}

/// Result of `POST /v1/onboard/exchange`.
///
/// `ready` carries everything the CLI needs to register a remote account:
/// the provider id, its credential fields, the wallet id and address. It is
/// returned once; the session is gone afterwards.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ExchangeResponse {
    Ready {
        provider: String,
        status: &'static str,
        network: &'static str,
        wallet_id: String,
        pubkey: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        credentials: std::collections::BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
    Pending {
        provider: &'static str,
        status: &'static str,
        email: String,
        network: &'static str,
        message: &'static str,
    },
}

/// JSON error envelope: `{ "error": "<code>", "message": "..." }`.
#[derive(Debug, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: StatusCode,
    pub error: &'static str,
    pub message: String,
    /// Machine-readable extras for errors the page acts on.
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

    /// The store cannot take another session right now.
    pub fn busy() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error: "busy",
            message: "Too many sign-ins are in progress. Try again in a few minutes.".to_string(),
            details: None,
        }
    }

    fn from_claim(err: crate::ClaimError) -> Self {
        match err {
            crate::ClaimError::Unknown => Self::bad_request(
                "unknown_session",
                "This sign-in does not match a pending `pay setup`. Run `pay setup` again.",
            ),
            crate::ClaimError::InProgress => Self {
                status: StatusCode::CONFLICT,
                error: "provisioning",
                message: "This sign-in is already being completed. Return to your terminal."
                    .to_string(),
                details: None,
            },
            crate::ClaimError::Completed => Self::bad_request(
                "already_completed",
                "This sign-in was already used. Return to your terminal.",
            ),
        }
    }

    fn from_driver(err: DriverError) -> Self {
        let status = match &err {
            DriverError::InvalidGrant(_) => StatusCode::BAD_REQUEST,
            DriverError::Rejected { .. } | DriverError::Protocol { .. } => StatusCode::BAD_GATEWAY,
            DriverError::Unreachable { .. } => StatusCode::BAD_GATEWAY,
            DriverError::Crypto(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let code = match &err {
            DriverError::InvalidGrant(_) => "invalid_consent",
            DriverError::Rejected { .. } => "provider_rejected",
            DriverError::Unreachable { .. } => "provider_unreachable",
            DriverError::Protocol { .. } => "provider_protocol",
            DriverError::Crypto(_) => "internal",
        };
        Self {
            status,
            error: code,
            message: err.to_string(),
            details: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

// ── Validation ────────────────────────────────────────────────────────────

/// One `@`, non-empty local and domain parts, no whitespace.
pub fn validate_email(email: &str) -> Result<(), ApiError> {
    let ok = !email.chars().any(char::is_whitespace)
        && email.matches('@').count() == 1
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty());
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "invalid_email",
            "Enter a valid email address.",
        ))
    }
}

/// `http://127.0.0.1:<port>/callback` or `http://localhost:<port>/callback`,
/// no query, no fragment, no credentials.
pub fn validate_callback(callback: &str) -> Result<Url, ApiError> {
    let err = |msg: &str| ApiError::bad_request("invalid_callback", msg);
    let url = Url::parse(callback).map_err(|e| err(&format!("callback is not a URL: {e}")))?;
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

/// `[A-Za-z0-9_-]+` (base64url alphabet, no padding).
pub fn is_base64url_alphabet(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_token(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len()) && is_base64url_alphabet(value)
}

/// 16..=128 base64url chars.
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

/// 43..=128 base64url chars (RFC 7636 §4.2).
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

// ── Crypto helpers ────────────────────────────────────────────────────────

/// 32 random bytes, base64url without padding (43 chars).
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// RFC 7636 S256: `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn verify_pkce(verifier: &str, challenge: &str) -> bool {
    pkce_challenge(verifier) == challenge
}

/// `<callback>?code=<code>&state=<state>` with URL-encoded values.
pub fn redirect_url(callback: &str, code: &str, state: &str) -> String {
    let mut url = Url::parse(callback).expect("callback validated before session creation");
    url.query_pairs_mut()
        .append_pair("code", code)
        .append_pair("state", state);
    url.to_string()
}

// ── Handlers ──────────────────────────────────────────────────────────────

fn parse_json<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request("invalid_request", format!("invalid JSON body: {e}")))
}

/// `POST /api/onboard/start`
///
/// With a `provider`, opens a session and returns the provider's consent
/// URL; the browser comes back through `/onboard/{provider}/callback` and
/// `/api/onboard/{provider}/complete`. Without one, the email-only stub
/// path returns straight to the CLI.
pub async fn start(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<StartResponse>, ApiError> {
    let req: StartRequest = parse_json(&body)?;
    if let Some(authorization) = req.authorization_request.as_deref() {
        return start_for_connector(&state, &req, authorization);
    }
    let missing = |field: &str| {
        ApiError::bad_request(
            "invalid_request",
            format!("`{field}` is required to link a terminal."),
        )
    };
    let callback = req.callback.clone().ok_or_else(|| missing("callback"))?;
    let state_param = req.state.clone().ok_or_else(|| missing("state"))?;
    let code_challenge = req
        .code_challenge
        .clone()
        .ok_or_else(|| missing("code_challenge"))?;
    validate_callback(&callback)?;
    validate_state(&state_param)?;
    validate_code_challenge(&code_challenge)?;
    if let Some(email) = req.email.as_deref() {
        validate_email(email)?;
    }

    let log_context = |session: &OnboardSession| {
        tracing::info!(
            provider = session.provider.as_deref().unwrap_or("-"),
            email = session.email.as_deref().unwrap_or("-"),
            account = req.account.as_deref().unwrap_or("-"),
            host = req.host.as_deref().unwrap_or("-"),
            cli = req.cli.as_deref().unwrap_or("-"),
            "onboarding started"
        );
    };

    match req.provider.as_deref() {
        Some(provider_id) => {
            let driver = state.driver(provider_id).ok_or_else(|| {
                ApiError::bad_request(
                    "unknown_provider",
                    format!("No wallet provider `{provider_id}` is available."),
                )
            })?;
            let session = OnboardSession::new(
                req.email.clone(),
                state_param.clone(),
                callback.clone(),
                code_challenge.clone(),
                Some(driver.id().to_string()),
            );
            let redirect_uri = state.consent_redirect_uri(driver.id());
            let consent = driver.consent_url(&redirect_uri, &session.state);
            log_context(&session);
            state
                .insert_session(session)
                .map_err(|_| ApiError::busy())?;
            Ok(Json(StartResponse {
                redirect: None,
                consent: Some(consent),
                provider: Some(driver.id().to_string()),
            }))
        }
        None => {
            let email = req.email.clone().ok_or_else(|| {
                ApiError::bad_request("invalid_email", "Enter a valid email address.")
            })?;
            let session = OnboardSession::new(
                Some(email),
                state_param.clone(),
                callback.clone(),
                code_challenge.clone(),
                None,
            );
            let redirect = session.redirect_url();
            log_context(&session);
            state
                .insert_session(session)
                .map_err(|_| ApiError::busy())?;
            Ok(Json(StartResponse {
                redirect: Some(redirect),
                consent: None,
                provider: None,
            }))
        }
    }
}

/// Connector origin of `POST /api/onboard/start`: the consent page asks
/// for a wallet before it can approve `authorization`.
fn start_for_connector(
    state: &AppState,
    req: &StartRequest,
    authorization: &str,
) -> Result<Json<StartResponse>, ApiError> {
    #[cfg(not(feature = "mcp"))]
    {
        let _ = (state, req, authorization);
        Err(ApiError::bad_request(
            "connector_disabled",
            "This server has no MCP connector.",
        ))
    }
    #[cfg(feature = "mcp")]
    {
        let oauth = state.oauth().ok_or_else(|| {
            ApiError::bad_request("connector_disabled", "This server has no MCP connector.")
        })?;
        if oauth.pending(authorization).is_none() {
            return Err(ApiError::bad_request(
                "unknown_request",
                "This sign-in request is unknown or has expired. Start again from your MCP client.",
            ));
        }
        let provider_id = req.provider.as_deref().ok_or_else(|| {
            ApiError::bad_request(
                "invalid_request",
                "`provider` is required to create a wallet.",
            )
        })?;
        let driver = state.driver(provider_id).ok_or_else(|| {
            ApiError::bad_request(
                "unknown_provider",
                format!("No wallet provider `{provider_id}` is available."),
            )
        })?;
        let session = OnboardSession::for_connector(authorization, driver.id());
        let consent = driver.consent_url(&state.consent_redirect_uri(driver.id()), &session.state);
        tracing::info!(
            provider = driver.id(),
            authorization,
            "connector onboarding started"
        );
        state
            .insert_session(session)
            .map_err(|_| ApiError::busy())?;
        Ok(Json(StartResponse {
            redirect: None,
            consent: Some(consent),
            provider: Some(driver.id().to_string()),
        }))
    }
}

/// `POST /api/onboard/{provider}/complete`
///
/// The consent page redirected the browser back with the grant in the URL
/// fragment; the page posts that fragment here. The driver turns it into a
/// wallet, the wallet is parked on the session, and the browser is sent to
/// the CLI's callback with the one-time code.
pub async fn complete(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let req: CompleteRequest = parse_json(&body)?;
    let driver = state.driver(&provider_id).ok_or_else(|| {
        ApiError::bad_request(
            "unknown_provider",
            format!("No wallet provider `{provider_id}` is available."),
        )
    })?;
    let (grant, echoed_state) = driver
        .parse_grant(&req.fragment)
        .map_err(ApiError::from_driver)?;

    // Hold the session for this run before touching the provider: two
    // completions racing here would otherwise create two wallets.
    let session = state
        .claim_provisioning(&echoed_state, driver.id())
        .map_err(ApiError::from_claim)?;

    // A connector user who already has a wallet under this provider account
    // gets it back, whatever browser they are in: no second wallet.
    #[cfg(feature = "mcp")]
    if let Some(authorization) = session.authorization.as_deref()
        && let Some(existing) = returning_tenant(&state, driver, &grant)
    {
        state.take_session(&session.code);
        return complete_for_returning_connector(&state, authorization, driver, &grant, existing);
    }

    // Separate OAuth requests for the same provider account have separate
    // onboarding sessions. Reserve the stable subject as well, so only one
    // of them may perform first-time external provisioning.
    #[cfg(feature = "mcp")]
    let _subject_claim = if session.authorization.is_some() {
        driver
            .account_identity(&grant)
            .map(|identity| {
                let subject = state.tenants().provider_subject(driver.id(), &identity);
                state.tenants().claim_provisioning(&subject).ok_or_else(|| {
                    state.release_provisioning(&echoed_state);
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "provisioning",
                        "This provider account is already being prepared. Try again in a moment.",
                    )
                })
            })
            .transpose()?
    } else {
        None
    };

    // A previous claimant may have bound the tenant just before this claim.
    #[cfg(feature = "mcp")]
    if let Some(authorization) = session.authorization.as_deref()
        && let Some(existing) = returning_tenant(&state, driver, &grant)
    {
        state.take_session(&session.code);
        return complete_for_returning_connector(&state, authorization, driver, &grant, existing);
    }

    let wallet = match driver.provision(&grant).await {
        Ok(wallet) => wallet,
        Err(err) => {
            state.release_provisioning(&echoed_state);
            return Err(ApiError::from_driver(err));
        }
    };
    let address = wallet.address.clone();
    tracing::info!(provider = driver.id(), address = %address, "wallet provisioned");

    if let Some(authorization) = session.authorization.as_deref() {
        return complete_for_connector(
            &state,
            &echoed_state,
            authorization,
            driver,
            &grant,
            wallet,
        );
    }

    let redirect = session.redirect_url();
    if !state.attach_wallet(&echoed_state, wallet) {
        return Err(ApiError::invalid_grant());
    }
    Ok(Json(CompleteResponse {
        origin: "cli",
        redirect,
        provider: driver.id().to_string(),
        address,
    })
    .into_response())
}

/// The tenant already bound to the provider account this grant belongs to.
#[cfg(feature = "mcp")]
fn returning_tenant(
    state: &AppState,
    driver: &dyn crate::drivers::WalletDriver,
    grant: &crate::drivers::ConsentGrant,
) -> Option<std::sync::Arc<crate::tenants::TenantRecord>> {
    #[cfg(feature = "mcp")]
    {
        let identity = driver.account_identity(grant)?;
        let subject = state.tenants().provider_subject(driver.id(), &identity);
        state.tenants().get(&subject)
    }
}

/// A returning connector user: approve for the existing subject, carry a
/// rotated key into the stored credentials, and refresh the browser cookie.
#[cfg(feature = "mcp")]
fn complete_for_returning_connector(
    state: &AppState,
    authorization: &str,
    driver: &dyn crate::drivers::WalletDriver,
    grant: &crate::drivers::ConsentGrant,
    existing: std::sync::Arc<crate::tenants::TenantRecord>,
) -> Result<Response, ApiError> {
    #[cfg(feature = "mcp")]
    {
        let oauth = state.oauth().ok_or_else(|| {
            ApiError::bad_request("connector_disabled", "This server has no MCP connector.")
        })?;
        state
            .tenants()
            .update_credentials(&existing.subject, |c| driver.refresh_credentials(c, grant));
        let (request, code) = oauth.approve(authorization, &existing.subject).ok_or_else(|| {
            ApiError::bad_request(
                "unknown_request",
                "The sign-in request expired while you signed in. Start again from your MCP client.",
            )
        })?;
        tracing::info!(
            subject = %existing.subject,
            client_id = %request.client_id,
            address = %existing.pubkey,
            "connector tenant recognised, wallet reused"
        );
        let mut response = Json(CompleteResponse {
            origin: "connector",
            redirect: crate::oauth::code_redirect(&request, &code),
            provider: driver.id().to_string(),
            address: existing.pubkey.clone(),
        })
        .into_response();
        response.headers_mut().insert(
            axum::http::header::SET_COOKIE,
            state.tenants().subject_cookie(
                &existing.subject,
                state.public_url().starts_with("https://"),
            ),
        );
        Ok(response)
    }
}

/// Connector origin of a completion: the wallet becomes a tenant for the
/// provider account's subject, the pending authorization is approved for
/// that subject, and the browser learns its subject so the next client
/// reuses the wallet without a provider round-trip.
fn complete_for_connector(
    state: &AppState,
    echoed_state: &str,
    authorization: &str,
    driver: &dyn crate::drivers::WalletDriver,
    grant: &crate::drivers::ConsentGrant,
    wallet: ProvisionedWallet,
) -> Result<Response, ApiError> {
    #[cfg(not(feature = "mcp"))]
    {
        let _ = (state, echoed_state, authorization, driver, grant, wallet);
        Err(ApiError::bad_request(
            "connector_disabled",
            "This server has no MCP connector.",
        ))
    }
    #[cfg(feature = "mcp")]
    {
        use crate::tenants::TenantRecord;
        let oauth = state.oauth().ok_or_else(|| {
            ApiError::bad_request("connector_disabled", "This server has no MCP connector.")
        })?;
        let address = wallet.address.clone();
        // Stable per provider account; a grant with no account identity
        // (a provider that has none) falls back to this wallet's address.
        let subject = match driver.account_identity(grant) {
            Some(identity) => state.tenants().provider_subject(driver.id(), &identity),
            None => state
                .tenants()
                .provider_subject(driver.id(), &format!("wallet:{address}")),
        };
        let record = TenantRecord::from_wallet(&subject, &wallet);
        if !state.attach_wallet(echoed_state, wallet) {
            return Err(ApiError::invalid_grant());
        }
        let (request, code) = oauth.approve(authorization, &subject).ok_or_else(|| {
            ApiError::bad_request(
                "unknown_request",
                "The sign-in request expired while the wallet was being created. Start again from your MCP client.",
            )
        })?;
        state.tenants().bind(record);
        tracing::info!(%subject, client_id = %request.client_id, address = %address, "connector tenant bound");
        let redirect = crate::oauth::code_redirect(&request, &code);
        let mut response = Json(CompleteResponse {
            origin: "connector",
            redirect,
            provider: driver.id().to_string(),
            address,
        })
        .into_response();
        response.headers_mut().insert(
            axum::http::header::SET_COOKIE,
            state
                .tenants()
                .subject_cookie(&subject, state.public_url().starts_with("https://")),
        );
        Ok(response)
    }
}

/// `POST /v1/onboard/exchange`
pub async fn exchange(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<ExchangeResponse>, ApiError> {
    let req: ExchangeRequest = parse_json(&body)?;
    // Single use: the session is removed on lookup regardless of the PKCE
    // outcome, so a wrong verifier burns the code.
    let session = state
        .take_session(&req.code)
        .ok_or_else(ApiError::invalid_grant)?;
    if !verify_pkce(&req.code_verifier, &session.code_challenge) {
        tracing::warn!(state = %session.state, "PKCE verifier mismatch");
        return Err(ApiError::invalid_grant());
    }
    tracing::info!(
        provider = session.provider.as_deref().unwrap_or("-"),
        ready = session.wallet.is_some(),
        "onboarding code exchanged"
    );
    Ok(Json(match session.wallet {
        Some(wallet) => ExchangeResponse::Ready {
            provider: wallet.provider.to_string(),
            status: "ready",
            network: NETWORK,
            wallet_id: wallet.wallet_id,
            pubkey: wallet.address,
            project_id: wallet.project_id,
            credentials: wallet.credentials,
            email: session.email,
        },
        None => ExchangeResponse::Pending {
            provider: "pay-cloud",
            status: "pending",
            email: session.email.unwrap_or_default(),
            network: NETWORK,
            message: "Wallet provisioning is not available yet.",
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn callback_accepts_loopback_http() {
        assert!(validate_callback("http://127.0.0.1:53211/callback").is_ok());
        assert!(validate_callback("http://localhost:8080/callback").is_ok());
        assert!(validate_callback("http://127.0.0.1/callback").is_ok());
    }

    #[test]
    fn callback_rejects_everything_else() {
        for bad in [
            "https://127.0.0.1:53211/callback",
            "http://example.com/callback",
            "http://127.0.0.2:53211/callback",
            "http://[::1]:53211/callback",
            "http://127.0.0.1:53211/other",
            "http://127.0.0.1:53211/callback/",
            "http://127.0.0.1:53211/callback?x=1",
            "http://127.0.0.1:53211/callback#frag",
            "http://user:pw@127.0.0.1:53211/callback",
            "127.0.0.1:53211/callback",
            "",
        ] {
            let err = validate_callback(bad).expect_err(bad);
            assert_eq!(err.error, "invalid_callback", "{bad}");
            assert_eq!(err.status, StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn email_validation() {
        assert!(validate_email("a@b.co").is_ok());
        assert!(validate_email("first.last+tag@example.com").is_ok());
        for bad in ["", "nope", "@b.co", "a@", "a@b@c.co", "a b@c.co", "a@b .co"] {
            assert_eq!(
                validate_email(bad).unwrap_err().error,
                "invalid_email",
                "{bad}"
            );
        }
    }

    #[test]
    fn state_alphabet_and_bounds() {
        assert!(validate_state(&"a".repeat(16)).is_ok());
        assert!(validate_state(&"a".repeat(128)).is_ok());
        assert!(validate_state("abcDEF012-_ghijk").is_ok());
        assert!(validate_state(&"a".repeat(15)).is_err());
        assert!(validate_state(&"a".repeat(129)).is_err());
        assert!(validate_state("abcdefghijklmno+").is_err());
        assert!(validate_state("abcdefghijklmno=").is_err());
        assert!(validate_state("abcdefghijklmno ").is_err());
    }

    #[test]
    fn challenge_alphabet_and_bounds() {
        assert!(validate_code_challenge(RFC_CHALLENGE).is_ok());
        assert!(validate_code_challenge(&"a".repeat(43)).is_ok());
        assert!(validate_code_challenge(&"a".repeat(128)).is_ok());
        assert!(validate_code_challenge(&"a".repeat(42)).is_err());
        assert!(validate_code_challenge(&"a".repeat(129)).is_err());
        assert!(validate_code_challenge(&format!("{}/", "a".repeat(42))).is_err());
    }

    #[test]
    fn pkce_matches_rfc_7636_vector() {
        assert_eq!(pkce_challenge(RFC_VERIFIER), RFC_CHALLENGE);
        assert!(verify_pkce(RFC_VERIFIER, RFC_CHALLENGE));
        assert!(!verify_pkce("wrong", RFC_CHALLENGE));
    }

    #[test]
    fn random_token_is_43_base64url_chars() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 43);
        assert!(is_base64url_alphabet(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn sha256_hex_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn redirect_url_encodes_values() {
        let url = redirect_url("http://127.0.0.1:53211/callback", "c/o+d=e", "s&t");
        assert_eq!(
            url,
            "http://127.0.0.1:53211/callback?code=c%2Fo%2Bd%3De&state=s%26t"
        );
    }

    fn session(created_at: Instant) -> OnboardSession {
        OnboardSession {
            code: random_token(),
            email: Some("a@b.co".into()),
            state: "s".repeat(16),
            callback: "http://127.0.0.1:1/callback".into(),
            code_challenge: RFC_CHALLENGE.into(),
            provider: None,
            wallet: None,
            provisioning: false,
            authorization: None,
            created_at,
        }
    }

    #[test]
    fn session_ttl_expiry() {
        let now = Instant::now();
        let fresh = session(now);
        assert!(!fresh.is_expired_at(now));
        assert!(!fresh.is_expired_at(now + SESSION_TTL));
        assert!(fresh.is_expired_at(now + SESSION_TTL + Duration::from_secs(1)));

        let state = AppState::with_drivers("http://cloud.test", Vec::new());
        let expired = session(
            now.checked_sub(SESSION_TTL + Duration::from_secs(1))
                .expect("clock far enough from epoch"),
        );
        let expired_code = expired.code.clone();
        state.insert_session(expired).unwrap();
        assert!(state.take_session(&expired_code).is_none());
    }

    /// Expired entries are only swept when the store is full (see
    /// `AppState::insert_session`); until then every lookup still treats
    /// them as gone.
    #[test]
    fn expired_sessions_are_invisible_before_they_are_swept() {
        let state = AppState::with_drivers("http://cloud.test", Vec::new());
        let now = Instant::now();
        let fresh_a = session(now);
        let code_a = fresh_a.code.clone();
        state.insert_session(fresh_a).unwrap();
        let expired = session(
            now.checked_sub(SESSION_TTL + Duration::from_secs(1))
                .unwrap(),
        );
        let code_expired = expired.code.clone();
        let expired_state = expired.state.clone();
        state.insert_session(expired).unwrap();
        assert_eq!(state.session_count(), 2);

        assert!(state.session_by_state(&expired_state).is_none());
        assert!(state.take_session(&code_expired).is_none());
        assert!(state.take_session(&code_a).is_some());
    }

    #[test]
    fn take_session_is_single_use() {
        let state = AppState::with_drivers("http://cloud.test", Vec::new());
        let s = session(Instant::now());
        let code = s.code.clone();
        state.insert_session(s).unwrap();
        assert!(state.take_session(&code).is_some());
        assert!(state.take_session(&code).is_none());
        assert!(state.take_session("unknown").is_none());
    }
}
