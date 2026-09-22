//! pay-cloud v0 — browser onboarding for the `pay` CLI.
//!
//! Serves the onboarding APIs used by the separately deployed pay-web-ui
//! frontend and these JSON endpoints:
//!
//! - `POST /api/onboard/start` — page → server: open a session; with a
//!   `provider`, returns that provider's consent URL.
//! - `POST /api/onboard/{provider}/complete` — page → server: the consent
//!   fragment; the driver provisions a wallet onto the session.
//! - `POST /v1/onboard/exchange` — CLI → server: redeem the code with PKCE
//!   and receive the wallet's credentials, once.
//!
//! State is in-memory; sessions expire after [`onboard::SESSION_TTL`].
//! Wallet drivers live in [`drivers`], each behind a cargo feature.

pub mod drivers;
pub mod onboard;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::response::Redirect;
use axum::routing::{get, post};
use serde_json::json;

pub use onboard::{OnboardSession, SESSION_TTL};

#[cfg(feature = "coinflow")]
pub mod funding;
#[cfg(feature = "mcp")]
pub mod hosts;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp")]
pub mod oauth;
#[cfg(feature = "privy")]
pub mod privy;
#[cfg(feature = "mcp")]
pub mod tenants;

const DEFAULT_PAGES_URL: &str = "https://pay.sh";

/// Most sessions held at once. A session is a few hundred bytes and lives
/// five minutes, so this bounds the store at a few megabytes while leaving
/// room for far more concurrent sign-ins than one server will see.
pub const MAX_SESSIONS: usize = 4096;

/// The session store is at capacity with live sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionsFull;

/// Why a session could not be reserved for provisioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// No live session for this state and provider.
    Unknown,
    /// Another completion for the same sign-in is provisioning right now.
    InProgress,
    /// The wallet was already provisioned; the code is waiting to be exchanged.
    Completed,
}

/// Shared server state: pending sessions keyed by `sha256(code)` hex, an
/// index from CLI `state` to that key (the consent page echoes `state`),
/// the compiled-in wallet drivers, and the public base URL consent pages
/// redirect back to.
#[derive(Clone)]
pub struct AppState {
    sessions: Arc<Mutex<HashMap<String, OnboardSession>>>,
    by_state: Arc<Mutex<HashMap<String, String>>>,
    drivers: Arc<Vec<Box<dyn drivers::WalletDriver>>>,
    public_url: String,
    /// The separately deployed pay.sh web app, which proxies its API calls
    /// back here.
    pages_url: String,
    /// Card purchases through Coinflow; `None` until configured.
    #[cfg(feature = "coinflow")]
    funding: Option<Arc<funding::Funding>>,
    /// The hosted MCP connector; `None` until enabled.
    #[cfg(feature = "mcp")]
    mcp: Option<Arc<mcp::Config>>,
    /// The connector's OAuth authorization server; mounted with the connector.
    #[cfg(feature = "mcp")]
    oauth: Option<Arc<oauth::Store>>,
    /// Wallets and policies behind connector subjects.
    #[cfg(feature = "mcp")]
    tenants: Arc<tenants::TenantRegistry>,
    /// Privy login on the consent page; `None` until configured.
    #[cfg(feature = "privy")]
    privy: Option<Arc<privy::Privy>>,
    /// Answers "does this wallet hold anything to pay with" after a sign-in.
    #[cfg(feature = "mcp")]
    wallet_probe: Arc<dyn WalletProbe>,
}

/// Whether a wallet holds any stablecoin, asked after a sign-in to decide
/// if the user is sent to the funding page first. pay-api in production;
/// tests inject a fixed answer.
#[cfg(feature = "mcp")]
#[async_trait::async_trait]
pub trait WalletProbe: Send + Sync {
    /// True when the wallet is known to hold nothing. Unsure means false:
    /// an outage must never insert a detour.
    async fn holds_nothing(&self, address: &str) -> bool;
}

/// pay-api's stablecoin balance endpoint (`PAY_API_URL`), five-second cap.
#[cfg(feature = "mcp")]
pub struct PayApiProbe;

#[cfg(feature = "mcp")]
#[async_trait::async_trait]
impl WalletProbe for PayApiProbe {
    async fn holds_nothing(&self, address: &str) -> bool {
        let rpc = pay_core::client::subscription::default_rpc_url_for_network(
            pay_core::accounts::MAINNET_NETWORK,
        );
        let lookup = pay_core::client::balance::get_stablecoin_balances(&rpc, address);
        match tokio::time::timeout(std::time::Duration::from_secs(5), lookup).await {
            Ok(Ok(balances)) => {
                !balances.tokens_unavailable && balances.tokens.iter().all(|t| t.raw_amount == 0)
            }
            Ok(Err(e)) => {
                tracing::info!(%address, error = %e, "balance lookup failed; assuming funded");
                false
            }
            Err(_) => {
                tracing::info!(%address, "balance lookup timed out; assuming funded");
                false
            }
        }
    }
}

/// A probe with a fixed answer, for tests.
#[cfg(feature = "mcp")]
pub struct FixedProbe(pub bool);

#[cfg(feature = "mcp")]
#[async_trait::async_trait]
impl WalletProbe for FixedProbe {
    async fn holds_nothing(&self, _address: &str) -> bool {
        self.0
    }
}

impl AppState {
    /// State with every compiled-in driver. `public_url` is how browsers
    /// reach this server, e.g. `https://cloud.pay.sh` or
    /// `http://127.0.0.1:8402`.
    pub fn new(public_url: impl Into<String>) -> Self {
        Self::with_drivers(public_url, drivers::all())
    }

    /// State with an explicit driver set (tests inject a fake).
    pub fn with_drivers(
        public_url: impl Into<String>,
        drivers: Vec<Box<dyn drivers::WalletDriver>>,
    ) -> Self {
        Self {
            sessions: Arc::default(),
            by_state: Arc::default(),
            drivers: Arc::new(drivers),
            public_url: public_url.into().trim_end_matches('/').to_string(),
            pages_url: DEFAULT_PAGES_URL.to_string(),
            #[cfg(feature = "coinflow")]
            funding: None,
            #[cfg(feature = "mcp")]
            mcp: None,
            #[cfg(feature = "mcp")]
            oauth: None,
            #[cfg(feature = "mcp")]
            tenants: Arc::default(),
            #[cfg(feature = "privy")]
            privy: None,
            #[cfg(feature = "mcp")]
            wallet_probe: Arc::new(PayApiProbe),
        }
    }

    /// Decide "empty wallet" differently (tests).
    #[cfg(feature = "mcp")]
    pub fn with_wallet_probe(mut self, probe: Arc<dyn WalletProbe>) -> Self {
        self.wallet_probe = probe;
        self
    }

    #[cfg(feature = "mcp")]
    pub fn wallet_probe(&self) -> &dyn WalletProbe {
        self.wallet_probe.as_ref()
    }

    /// Offer Privy login on the consent page, with the user's Privy wallet
    /// as the tenant's wallet.
    #[cfg(feature = "privy")]
    pub fn with_privy(mut self, privy: privy::Privy) -> Self {
        self.privy = Some(Arc::new(privy));
        self
    }

    #[cfg(feature = "privy")]
    pub fn privy(&self) -> Option<&privy::Privy> {
        self.privy.as_deref()
    }

    #[cfg(feature = "mcp")]
    pub fn tenants(&self) -> &Arc<tenants::TenantRegistry> {
        &self.tenants
    }

    /// Serve the MCP connector at `/mcp` and its OAuth server.
    #[cfg(feature = "mcp")]
    pub fn with_mcp(mut self, cfg: mcp::Config) -> Self {
        self.oauth = Some(Arc::new(oauth::Store::new(&cfg.public_url)));
        self.mcp = Some(Arc::new(cfg));
        self
    }

    #[cfg(feature = "mcp")]
    pub fn oauth(&self) -> Option<&oauth::Store> {
        self.oauth.as_deref()
    }

    /// Enable card purchases with this Coinflow merchant.
    #[cfg(feature = "coinflow")]
    pub fn with_funding(mut self, funding: funding::Funding) -> Self {
        self.funding = Some(Arc::new(funding));
        self
    }

    #[cfg(feature = "coinflow")]
    pub fn funding(&self) -> Option<&funding::Funding> {
        self.funding.as_deref()
    }

    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    /// Serve browser pages from `url` (the pay.sh web app). That app proxies
    /// `/api/oauth/*` and `/api/fund/*` here.
    pub fn with_pages_url(mut self, url: impl Into<String>) -> Self {
        self.pages_url = url.into().trim_end_matches('/').to_string();
        self
    }

    /// The consent page for a pending authorization: `/connect` on the
    /// separately deployed pages app.
    pub fn consent_page_url(&self, request_id: &str) -> String {
        format!("{}?request={request_id}", self.consent_page())
    }

    /// The consent page itself, where a guest also attaches a wallet
    /// (`?link=<ticket>`).
    pub fn consent_page(&self) -> String {
        format!("{}/connect", self.pages_url())
    }

    fn pages_url(&self) -> &str {
        &self.pages_url
    }

    /// Where a provider's consent page should send the browser back.
    pub fn consent_redirect_uri(&self, provider_id: &str) -> String {
        format!("{}/onboard/{provider_id}/callback", self.public_url)
    }

    pub fn driver(&self, id: &str) -> Option<&dyn drivers::WalletDriver> {
        self.drivers
            .iter()
            .find(|d| d.id() == id)
            .map(|d| d.as_ref())
    }

    pub fn driver_ids(&self) -> Vec<&'static str> {
        self.drivers.iter().map(|d| d.id()).collect()
    }

    /// Store a session. The store is bounded: when it is full, expired
    /// sessions are swept, and if it is still full the session is refused.
    /// Anyone can call `start`, so this is what keeps memory finite under
    /// a flood; live sessions are never evicted for new ones.
    pub fn insert_session(&self, session: OnboardSession) -> Result<(), SessionsFull> {
        let mut sessions = self.sessions.lock().unwrap();
        let mut by_state = self.by_state.lock().unwrap();
        if sessions.len() >= MAX_SESSIONS {
            let now = Instant::now();
            sessions.retain(|_, s| !s.is_expired_at(now));
            by_state.retain(|_, key| sessions.contains_key(key));
            if sessions.len() >= MAX_SESSIONS {
                return Err(SessionsFull);
            }
        }
        by_state.insert(session.state.clone(), session.key());
        sessions.insert(session.key(), session);
        Ok(())
    }

    /// Remove and return the session for `code`. `None` when unknown or
    /// expired (an expired hit is dropped too).
    pub fn take_session(&self, code: &str) -> Option<OnboardSession> {
        let key = onboard::sha256_hex(code);
        let session = self.sessions.lock().unwrap().remove(&key)?;
        self.by_state.lock().unwrap().remove(&session.state);
        (!session.is_expired_at(Instant::now())).then_some(session)
    }

    /// A live session by its CLI `state`, cloned.
    pub fn session_by_state(&self, state: &str) -> Option<OnboardSession> {
        let key = self.by_state.lock().unwrap().get(state)?.clone();
        let session = self.sessions.lock().unwrap().get(&key)?.clone();
        (!session.is_expired_at(Instant::now())).then_some(session)
    }

    /// Reserve the session for `state` for one provisioning run by
    /// `provider`, returning a copy of it. Provisioning creates a wallet
    /// at the provider, which cannot be undone, so exactly one completion
    /// may run per session: a second concurrent one is told it is in
    /// progress, and a later one that it is already done.
    pub fn claim_provisioning(
        &self,
        state: &str,
        provider: &str,
    ) -> Result<OnboardSession, ClaimError> {
        let Some(key) = self.by_state.lock().unwrap().get(state).cloned() else {
            return Err(ClaimError::Unknown);
        };
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(&key).ok_or(ClaimError::Unknown)?;
        if session.is_expired_at(Instant::now()) || session.provider.as_deref() != Some(provider) {
            return Err(ClaimError::Unknown);
        }
        if session.wallet.is_some() {
            return Err(ClaimError::Completed);
        }
        if session.provisioning {
            return Err(ClaimError::InProgress);
        }
        session.provisioning = true;
        Ok(session.clone())
    }

    /// Undo [`claim_provisioning`](Self::claim_provisioning) after the
    /// provider failed, so the user can retry the sign-in.
    pub fn release_provisioning(&self, state: &str) {
        let Some(key) = self.by_state.lock().unwrap().get(state).cloned() else {
            return;
        };
        if let Some(session) = self.sessions.lock().unwrap().get_mut(&key) {
            session.provisioning = false;
        }
    }

    /// Park a provisioned wallet on the session for `state`, ending its
    /// claim. False when the session is gone.
    pub fn attach_wallet(&self, state: &str, wallet: drivers::ProvisionedWallet) -> bool {
        let Some(key) = self.by_state.lock().unwrap().get(state).cloned() else {
            return false;
        };
        match self.sessions.lock().unwrap().get_mut(&key) {
            Some(session) => {
                session.wallet = Some(wallet);
                session.provisioning = false;
                true
            }
            None => false,
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

/// Full pay-cloud router: health and API endpoints. Browser routes redirect
/// to the separately deployed pages app.
pub fn router(state: AppState) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route("/api/onboard/start", post(onboard::start))
        .route("/api/onboard/{provider}/complete", post(onboard::complete))
        .route("/v1/onboard/exchange", post(onboard::exchange))
        .route("/", get(redirect_pages_root))
        .route("/onboard", get(redirect_onboard))
        .route("/onboard/{*rest}", get(redirect_onboard))
        .route("/fund", get(redirect_fund))
        .route("/authorize", get(redirect_authorize));
    // Metadata at the root and at the RFC 9728 / RFC 8414 path-based
    // locations for the `/mcp` resource; hosts try either. OpenID discovery
    // too, since some clients start there.
    #[cfg(feature = "mcp")]
    let router = router
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server/mcp",
            get(oauth::metadata),
        )
        .route("/.well-known/openid-configuration", get(oauth::metadata))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth::protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(oauth::protected_resource),
        )
        .route("/oauth/register", post(oauth::register))
        .route("/oauth/authorize", get(oauth::authorize))
        .route("/oauth/token", post(oauth::token))
        .route("/oauth/revoke", post(oauth::revoke))
        .route("/api/oauth/authorize/{request}", get(oauth::pending_view))
        .route(
            "/api/oauth/authorize/{request}/approve",
            post(oauth::approve),
        )
        .route("/api/oauth/authorize/{request}/deny", post(oauth::deny))
        .route("/api/oauth/link/{ticket}", get(oauth::link_view))
        .route("/api/oauth/link/{ticket}", post(oauth::link_complete))
        .route("/api/session/logout", post(oauth::sign_out));
    #[cfg(feature = "coinflow")]
    let router = router
        .route("/api/fund/start", post(funding::start))
        .route("/api/fund/webhook", post(funding::webhook))
        .route("/api/fund/{payment_id}", get(funding::status));
    #[cfg(feature = "mcp")]
    let mcp = state.mcp.clone().map(|cfg| {
        (
            mcp::Auth {
                cfg,
                oauth: state.oauth.clone(),
            },
            Arc::new(
                tenants::CloudContext::new(state.tenants.clone())
                    .with_link_page(state.consent_page()),
            ) as Arc<dyn pay_mcp::PayContext>,
        )
    });
    let router = router.with_state(state);
    #[cfg(feature = "mcp")]
    let router = match mcp {
        Some((auth, context)) => router.merge(mcp::router(auth, context)),
        None => router,
    };
    // Hosts and their browsers call the OAuth endpoints cross-origin;
    // sessions ride on `mcp-session-id`, which the browser must be allowed
    // to read back.
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::ACCEPT,
            axum::http::HeaderName::from_static("mcp-session-id"),
            axum::http::HeaderName::from_static("mcp-protocol-version"),
            axum::http::HeaderName::from_static("last-event-id"),
        ])
        .expose_headers([
            axum::http::header::WWW_AUTHENTICATE,
            axum::http::HeaderName::from_static("mcp-session-id"),
        ])
        .max_age(std::time::Duration::from_secs(600));
    router.fallback(get(not_found)).layer(cors)
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(json!({ "status": "ok" }))
}

async fn redirect_pages_root(State(state): State<AppState>) -> Redirect {
    Redirect::temporary(state.pages_url())
}

async fn redirect_onboard(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
) -> Redirect {
    redirect_to_page(&state, uri.path(), uri.query())
}

async fn redirect_fund(State(state): State<AppState>, OriginalUri(uri): OriginalUri) -> Redirect {
    redirect_to_page(&state, "/onramp", uri.query())
}

async fn redirect_authorize(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
) -> Redirect {
    redirect_to_page(&state, "/connect", uri.query())
}

fn redirect_to_page(state: &AppState, path: &str, query: Option<&str>) -> Redirect {
    let mut target = format!("{}{path}", state.pages_url());
    if let Some(query) = query {
        target.push('?');
        target.push_str(query);
    }
    Redirect::temporary(&target)
}

async fn not_found(OriginalUri(uri): OriginalUri) -> (StatusCode, axum::Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({
            "error": "not_found",
            "message": format!("No route for {}.", uri.path()),
        })),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, header};
    use serde_json::Value;
    use tower::ServiceExt;

    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const STATE: &str = "abcdefghijklmnopqrstuvwxyz012345";
    const CALLBACK: &str = "http://127.0.0.1:53211/callback";
    const PUBLIC_URL: &str = "https://cloud.test";

    /// A driver that hands back a fixed wallet, or fails when the api key is
    /// `sk_fail`.
    pub(crate) struct FakeDriver;

    #[async_trait::async_trait]
    impl drivers::WalletDriver for FakeDriver {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn display_name(&self) -> &'static str {
            "Fake custody"
        }
        fn account_identity(&self, grant: &drivers::ConsentGrant) -> Option<String> {
            Some(grant.api_key.clone())
        }
        fn consent_url(&self, redirect_uri: &str, state: &str) -> String {
            format!("https://fake.test/consent?redirect_uri={redirect_uri}&state={state}")
        }
        fn parse_grant(
            &self,
            fragment: &str,
        ) -> Result<(drivers::ConsentGrant, String), drivers::DriverError> {
            let mut grant = drivers::ConsentGrant::default();
            let mut state = None;
            for (k, v) in url::form_urlencoded::parse(fragment.trim_start_matches('#').as_bytes()) {
                match k.as_ref() {
                    "api_key" => grant.api_key = v.into_owned(),
                    "project_id" => grant.project_id = Some(v.into_owned()),
                    "state" => state = Some(v.into_owned()),
                    _ => {}
                }
            }
            let state =
                state.ok_or_else(|| drivers::DriverError::InvalidGrant("no state".into()))?;
            Ok((grant, state))
        }
        fn refresh_credentials(
            &self,
            credentials: &mut std::collections::BTreeMap<String, String>,
            grant: &drivers::ConsentGrant,
        ) {
            credentials.insert("secret_key".to_string(), grant.api_key.clone());
        }
        async fn provision(
            &self,
            grant: &drivers::ConsentGrant,
        ) -> Result<drivers::ProvisionedWallet, drivers::DriverError> {
            if grant.api_key == "sk_fail" {
                return Err(drivers::DriverError::Rejected {
                    provider: "fake",
                    status: 401,
                    message: "bad key".into(),
                });
            }
            let mut credentials = std::collections::BTreeMap::new();
            credentials.insert("secret_key".to_string(), grant.api_key.clone());
            credentials.insert("wallet_secret".to_string(), "ws-b64".to_string());
            Ok(drivers::ProvisionedWallet {
                provider: "fake",
                credentials,
                wallet_id: "acc_fake".into(),
                address: "Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS".into(),
                project_id: Some("pro_1".into()),
            })
        }
    }

    fn test_state() -> AppState {
        AppState::with_drivers(PUBLIC_URL, vec![Box::new(FakeDriver)])
    }

    async fn call(
        app: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        let body = match body {
            Some(json) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&json).unwrap())
            }
            None => Body::empty(),
        };
        let res = app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    fn start_body() -> Value {
        json!({
            "email": "a@b.co",
            "callback": CALLBACK,
            "state": STATE,
            "code_challenge": RFC_CHALLENGE,
            "account": "default",
            "host": "test-host",
            "cli": "0.0.0",
        })
    }

    #[tokio::test]
    async fn health_ok() {
        let app = router(test_state());
        let (status, body) = call(&app, Method::GET, "/health", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "status": "ok" }));
    }

    #[tokio::test]
    async fn start_then_exchange_end_to_end() {
        let state = test_state();
        let app = router(state.clone());

        let (status, body) =
            call(&app, Method::POST, "/api/onboard/start", Some(start_body())).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let redirect = body["redirect"].as_str().expect("redirect");
        let url = url::Url::parse(redirect).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(53211));
        assert_eq!(url.path(), "/callback");
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs["state"], STATE);
        let code = pairs["code"].clone();
        assert_eq!(code.len(), 43);
        assert!(onboard::is_base64url_alphabet(&code));
        assert_eq!(state.session_count(), 1);

        // Wrong verifier burns the code.
        let (status, body) = call(
            &app,
            Method::POST,
            "/v1/onboard/exchange",
            Some(json!({ "code": code, "code_verifier": "not-the-verifier" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_grant");
        assert_eq!(state.session_count(), 0);

        // Fresh start, correct verifier.
        let (_, body) = call(&app, Method::POST, "/api/onboard/start", Some(start_body())).await;
        let url = url::Url::parse(body["redirect"].as_str().unwrap()).unwrap();
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        let (status, body) = call(
            &app,
            Method::POST,
            "/v1/onboard/exchange",
            Some(json!({ "code": code, "code_verifier": RFC_VERIFIER })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({
                "provider": "pay-cloud",
                "status": "pending",
                "email": "a@b.co",
                "network": "mainnet",
                "message": "Wallet provisioning is not available yet.",
            })
        );

        // Single use.
        let (status, body) = call(
            &app,
            Method::POST,
            "/v1/onboard/exchange",
            Some(json!({ "code": code, "code_verifier": RFC_VERIFIER })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn provider_flow_start_complete_exchange_hands_over_credentials() {
        let state = test_state();
        let app = router(state.clone());

        // start with a provider → consent URL pointing back at our callback
        let mut body = start_body();
        body.as_object_mut().unwrap().remove("email");
        body["provider"] = json!("fake");
        let (status, res) = call(&app, Method::POST, "/api/onboard/start", Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(res["provider"], "fake");
        assert!(res.get("redirect").is_none());
        let consent = res["consent"].as_str().unwrap();
        assert!(
            consent.contains(&format!("redirect_uri={PUBLIC_URL}/onboard/fake/callback")),
            "{consent}"
        );
        assert!(consent.contains(&format!("state={STATE}")));

        // unknown provider is refused
        let mut bad = start_body();
        bad["provider"] = json!("nope");
        let (status, res) = call(&app, Method::POST, "/api/onboard/start", Some(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(res["error"], "unknown_provider");

        // complete with a fragment for an unknown state → unknown_session
        let (status, res) = call(
            &app,
            Method::POST,
            "/api/onboard/fake/complete",
            Some(json!({ "fragment": "#api_key=sk_ok&state=notastate1234567" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{res}");
        assert_eq!(res["error"], "unknown_session");

        // provider failure surfaces as 502 and leaves the session usable
        let (status, res) = call(
            &app,
            Method::POST,
            "/api/onboard/fake/complete",
            Some(json!({ "fragment": format!("#api_key=sk_fail&state={STATE}") })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{res}");
        assert_eq!(res["error"], "provider_rejected");

        // complete for real → redirect to the CLI with the code
        let (status, res) = call(
            &app,
            Method::POST,
            "/api/onboard/fake/complete",
            Some(json!({ "fragment": format!("#api_key=sk_ok&state={STATE}") })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(res["provider"], "fake");
        assert_eq!(
            res["address"],
            "Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS"
        );
        let url = url::Url::parse(res["redirect"].as_str().unwrap()).unwrap();
        assert_eq!(url.path(), "/callback");
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())
            .unwrap();

        // second completion is refused
        let (status, res) = call(
            &app,
            Method::POST,
            "/api/onboard/fake/complete",
            Some(json!({ "fragment": format!("#api_key=sk_ok&state={STATE}") })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{res}");
        assert_eq!(res["error"], "already_completed");

        // exchange → ready result with the credentials, once
        let (status, res) = call(
            &app,
            Method::POST,
            "/v1/onboard/exchange",
            Some(json!({ "code": code, "code_verifier": RFC_VERIFIER })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(
            res,
            json!({
                "provider": "fake",
                "status": "ready",
                "network": "mainnet",
                "wallet_id": "acc_fake",
                "pubkey": "Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS",
                "project_id": "pro_1",
                "credentials": { "secret_key": "sk_ok", "wallet_secret": "ws-b64" },
            })
        );
        assert_eq!(state.session_count(), 0);
        assert!(state.session_by_state(STATE).is_none());
    }

    #[tokio::test]
    async fn start_rejects_invalid_input_with_json_errors() {
        let app = router(test_state());

        let mut bad_callback = start_body();
        bad_callback["callback"] = json!("https://evil.example/callback");
        let (status, body) =
            call(&app, Method::POST, "/api/onboard/start", Some(bad_callback)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_callback");
        assert!(body["message"].is_string());

        let mut bad_email = start_body();
        bad_email["email"] = json!("nope");
        let (status, body) = call(&app, Method::POST, "/api/onboard/start", Some(bad_email)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_email");

        let mut short_state = start_body();
        short_state["state"] = json!("short");
        let (_, body) = call(&app, Method::POST, "/api/onboard/start", Some(short_state)).await;
        assert_eq!(body["error"], "invalid_state");

        let mut short_challenge = start_body();
        short_challenge["code_challenge"] = json!("short");
        let (_, body) = call(
            &app,
            Method::POST,
            "/api/onboard/start",
            Some(short_challenge),
        )
        .await;
        assert_eq!(body["error"], "invalid_code_challenge");

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/onboard/start")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn exchange_unknown_code_is_invalid_grant() {
        let app = router(test_state());
        let (status, body) = call(
            &app,
            Method::POST,
            "/v1/onboard/exchange",
            Some(json!({ "code": "nope", "code_verifier": RFC_VERIFIER })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn browser_routes_redirect_to_the_pages_app() {
        let app = router(test_state().with_pages_url("https://pages.test"));
        for (path, expected) in [
            ("/", "https://pages.test"),
            ("/onboard", "https://pages.test/onboard"),
            (
                "/onboard/anything?x=1",
                "https://pages.test/onboard/anything?x=1",
            ),
            ("/fund?address=abc", "https://pages.test/onramp?address=abc"),
            (
                "/authorize?request=req",
                "https://pages.test/connect?request=req",
            ),
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT, "{path}");
            assert_eq!(res.headers()[header::LOCATION], expected, "{path}");
        }

        let missing = app
            .oneshot(
                Request::builder()
                    .uri("/some/unknown/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    /// A driver whose provisioning blocks until the test opens the gate,
    /// counting how many times it ran.
    struct GatedDriver {
        gate: Arc<tokio::sync::Notify>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl drivers::WalletDriver for GatedDriver {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn display_name(&self) -> &'static str {
            "Gated custody"
        }
        fn consent_url(&self, redirect_uri: &str, state: &str) -> String {
            FakeDriver.consent_url(redirect_uri, state)
        }
        fn parse_grant(
            &self,
            fragment: &str,
        ) -> Result<(drivers::ConsentGrant, String), drivers::DriverError> {
            FakeDriver.parse_grant(fragment)
        }
        async fn provision(
            &self,
            grant: &drivers::ConsentGrant,
        ) -> Result<drivers::ProvisionedWallet, drivers::DriverError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.gate.notified().await;
            FakeDriver.provision(grant).await
        }
    }

    /// Two completions for one sign-in must provision exactly one wallet:
    /// the second is refused while the first is still at the provider.
    #[tokio::test]
    async fn concurrent_completions_provision_once() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = AppState::with_drivers(
            PUBLIC_URL,
            vec![Box::new(GatedDriver {
                gate: gate.clone(),
                calls: calls.clone(),
            })],
        );
        let app = router(state.clone());
        let mut body = start_body();
        body["provider"] = json!("fake");
        let (status, _) = call(&app, Method::POST, "/api/onboard/start", Some(body)).await;
        assert_eq!(status, StatusCode::OK);

        let complete_body = json!({ "fragment": format!("#api_key=sk_ok&state={STATE}") });
        let first = tokio::spawn({
            let app = app.clone();
            let body = complete_body.clone();
            async move { call(&app, Method::POST, "/api/onboard/fake/complete", Some(body)).await }
        });
        // Let the first completion reach the provider and park there.
        while calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }

        let (status, res) = call(
            &app,
            Method::POST,
            "/api/onboard/fake/complete",
            Some(complete_body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{res}");
        assert_eq!(res["error"], "provisioning");

        gate.notify_one();
        let (status, res) = first.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Done: a third attempt is told so, and the code exchanges as ready.
        let (status, res) = call(
            &app,
            Method::POST,
            "/api/onboard/fake/complete",
            Some(complete_body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{res}");
        assert_eq!(res["error"], "already_completed");
        let session = state.session_by_state(STATE).unwrap();
        assert!(session.wallet.is_some());
        assert!(!session.provisioning);
    }

    fn session_with_state(state: &str) -> OnboardSession {
        OnboardSession::new(
            None,
            state.to_string(),
            CALLBACK.to_string(),
            RFC_CHALLENGE.to_string(),
            Some("fake".to_string()),
        )
    }

    #[tokio::test]
    async fn start_is_refused_when_the_store_is_full_of_live_sessions() {
        let state = test_state();
        for i in 0..MAX_SESSIONS {
            state
                .insert_session(session_with_state(&format!("state-{i:0>26}")))
                .unwrap();
        }
        assert_eq!(state.session_count(), MAX_SESSIONS);
        assert_eq!(
            state.insert_session(session_with_state(STATE)),
            Err(SessionsFull)
        );

        let app = router(state.clone());
        let (status, res) =
            call(&app, Method::POST, "/api/onboard/start", Some(start_body())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{res}");
        assert_eq!(res["error"], "busy");
        assert_eq!(state.session_count(), MAX_SESSIONS, "nothing was evicted");
    }

    #[test]
    fn a_full_store_sweeps_expired_sessions_before_refusing() {
        let state = test_state();
        let expired_at = Instant::now()
            .checked_sub(onboard::SESSION_TTL + std::time::Duration::from_secs(1))
            .unwrap();
        for i in 0..MAX_SESSIONS {
            let mut session = session_with_state(&format!("state-{i:0>26}"));
            session.created_at = expired_at;
            state.insert_session(session).unwrap();
        }
        state.insert_session(session_with_state(STATE)).unwrap();
        assert_eq!(state.session_count(), 1, "only the live session remains");
        assert!(state.session_by_state(STATE).is_some());
        assert!(
            state
                .session_by_state("state-00000000000000000000000000")
                .is_none()
        );
    }
}
