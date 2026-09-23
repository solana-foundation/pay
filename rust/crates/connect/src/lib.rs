//! pay-connect — Privy-backed wallets for MCP hosts and the `pay` CLI.
//!
//! The separately deployed pay-web-ui hosts the shared `/connect` page. This
//! service owns the authenticated APIs behind it:
//!
//! - `GET /v1/cli` creates a PKCE-bound terminal link and redirects to the UI.
//! - `POST /v1/cli/complete` consumes its one-time code and returns only a
//!   tenant-scoped pay-connect token.
//! - `/v1/wallets/*` lets that scoped token reach the tenant's hosted wallet.
//! - `/oauth/*` and `/mcp` serve MCP connector authentication and tools.
//!
//! Privy operator credentials remain server-side. State is currently in
//! memory and bounded; pending CLI requests expire in minutes.

use std::sync::Arc;

use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::response::Redirect;
use axum::routing::{get, post};
use serde_json::json;

#[cfg(feature = "mcp")]
pub mod cli;
#[cfg(feature = "mcp")]
pub mod hosts;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp")]
pub mod oauth;
#[cfg(feature = "privy")]
pub mod privy;
pub mod protocol;
#[cfg(feature = "mcp")]
pub mod tenants;
#[cfg(feature = "mcp")]
pub mod wallet_api;

const DEFAULT_PAGES_URL: &str = "https://pay.sh";

/// Shared server state for OAuth, CLI links, tenants, and Privy.
#[derive(Clone)]
pub struct AppState {
    public_url: String,
    /// The separately deployed pay.sh web app, which proxies its API calls
    /// back here.
    pages_url: String,
    /// The hosted MCP connector; `None` until enabled.
    #[cfg(feature = "mcp")]
    mcp: Option<Arc<mcp::Config>>,
    /// The connector's OAuth authorization server; mounted with the connector.
    #[cfg(feature = "mcp")]
    oauth: Option<Arc<oauth::Store>>,
    /// Wallets and policies behind connector subjects.
    #[cfg(feature = "mcp")]
    tenants: Arc<tenants::TenantRegistry>,
    /// Pending CLI links, one-time grants, and hashed CLI access tokens.
    #[cfg(feature = "mcp")]
    cli: Arc<cli::Store>,
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
            Ok(Ok(balances)) => balances_hold_nothing(&balances),
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

#[cfg(feature = "mcp")]
fn balances_hold_nothing(balances: &pay_core::client::balance::AccountBalances) -> bool {
    !balances.tokens_unavailable
        && !balances.credits_unavailable
        && balances.tokens.iter().all(|token| token.raw_amount == 0)
        && balances.credits.iter().all(|credit| credit.raw_amount == 0)
}

#[cfg(all(test, feature = "mcp"))]
mod balance_probe_tests {
    use super::balances_hold_nothing;
    use pay_core::client::balance::{AccountBalances, CreditBalance};

    #[test]
    fn known_empty_wallet_holds_nothing() {
        assert!(balances_hold_nothing(&AccountBalances::default()));
    }

    #[test]
    fn credit_only_wallet_does_not_look_empty() {
        let balances = AccountBalances {
            credits: vec![CreditBalance {
                program_id: "credit-program".into(),
                accounts: vec!["allowance-account".into()],
                currency: "USD".into(),
                raw_amount: 1,
                ui_amount: 0.000_001,
            }],
            ..Default::default()
        };

        assert!(!balances_hold_nothing(&balances));
    }

    #[test]
    fn unavailable_credits_do_not_look_empty() {
        let balances = AccountBalances {
            credits_unavailable: true,
            ..Default::default()
        };

        assert!(!balances_hold_nothing(&balances));
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
    /// `public_url` is how browsers reach this server.
    pub fn new(public_url: impl Into<String>) -> Self {
        Self {
            public_url: public_url.into().trim_end_matches('/').to_string(),
            pages_url: DEFAULT_PAGES_URL.to_string(),
            #[cfg(feature = "mcp")]
            mcp: None,
            #[cfg(feature = "mcp")]
            oauth: None,
            #[cfg(feature = "mcp")]
            tenants: Arc::default(),
            #[cfg(feature = "mcp")]
            cli: Arc::default(),
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

    #[cfg(feature = "mcp")]
    pub fn cli(&self) -> &cli::Store {
        &self.cli
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

    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    /// Serve browser pages from `url` (the pay.sh web app). That app proxies
    /// `/api/oauth/*` here. Funding APIs are owned by pay-web-ui.
    pub fn with_pages_url(mut self, url: impl Into<String>) -> Self {
        self.pages_url = url.into().trim_end_matches('/').to_string();
        self
    }

    /// The consent page for a pending authorization: `/connect` on the
    /// separately deployed pages app.
    pub fn consent_page_url(&self, request_id: &str) -> String {
        format!("{}?request={request_id}", self.consent_page())
    }

    /// The shared connect page in CLI-linking mode.
    #[cfg(feature = "mcp")]
    pub fn cli_page_url(&self, request_id: &str) -> String {
        format!("{}?cli={request_id}", self.consent_page())
    }

    /// The consent page itself, where a guest also attaches a wallet
    /// (`?link=<ticket>`).
    pub fn consent_page(&self) -> String {
        format!("{}/connect", self.pages_url())
    }

    fn pages_url(&self) -> &str {
        &self.pages_url
    }
}

/// Full pay-connect router: health and API endpoints. Browser routes redirect
/// to the separately deployed pages app.
pub fn router(state: AppState) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route("/", get(redirect_pages_root))
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
        .route("/v1/cli", get(cli::start).delete(cli::revoke))
        .route("/v1/cli/complete", post(cli::complete))
        .route("/api/cli/{request}", get(cli::pending_view))
        .route("/api/cli/{request}/approve", post(cli::approve))
        .route("/api/cli/{request}/deny", post(cli::deny))
        .route("/v1/wallets", get(wallet_api::list))
        .route(
            "/v1/wallets/{wallet}/sign-transaction",
            post(wallet_api::sign_transaction),
        )
        .route(
            "/v1/wallets/{wallet}/sign-message",
            post(wallet_api::sign_message),
        )
        .route("/api/session/logout", post(oauth::sign_out));
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
