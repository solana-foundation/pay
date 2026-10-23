//! Tests for server modules: proxy forwarding, payment middleware, accounting.
//!
//! Run: `cargo test -p pay-core --features server --test server_tests`

#![cfg(feature = "server")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use pay_core::PaymentState;
use pay_core::server::accounting::{AccountingKey, AccountingStore, InMemoryStore};
use pay_core::server::proxy;
use pay_core::server::session::SessionMpp;
use pay_kit::mpp::server::Mpp;
use pay_kit::mpp::server::session::SessionConfig;
use pay_types::metering::ApiSpec;
use serde_json::json;
use std::sync::Arc;

// ── Test app state ──

#[derive(Clone)]
struct TestState {
    apis: Arc<Vec<ApiSpec>>,
    mpp: Option<Mpp>,
}

impl PaymentState for TestState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        self.mpp.as_ref()
    }
}

#[derive(Clone)]
struct MultiCurrencyTestState {
    apis: Arc<Vec<ApiSpec>>,
    mpps: Vec<Mpp>,
}

impl PaymentState for MultiCurrencyTestState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        self.mpps.first()
    }
    fn mpps(&self) -> Vec<&Mpp> {
        self.mpps.iter().collect()
    }
}

#[derive(Clone)]
struct SessionTestState {
    apis: Arc<Vec<ApiSpec>>,
    mpp: Option<Mpp>,
    session_mpp: Option<Arc<SessionMpp>>,
}

#[derive(Clone)]
struct MultiCurrencySessionTestState {
    apis: Arc<Vec<ApiSpec>>,
    session_mpps: Vec<Arc<SessionMpp>>,
}

impl PaymentState for MultiCurrencySessionTestState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        None
    }
    fn session_mpp(&self) -> Option<&SessionMpp> {
        self.session_mpps.first().map(Arc::as_ref)
    }
    fn session_mpp_handle(&self) -> Option<Arc<SessionMpp>> {
        self.session_mpps.first().cloned()
    }
    fn session_mpps(&self) -> Vec<&SessionMpp> {
        self.session_mpps.iter().map(Arc::as_ref).collect()
    }
    fn session_mpp_handles(&self) -> Vec<Arc<SessionMpp>> {
        self.session_mpps.clone()
    }
}

impl PaymentState for SessionTestState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        self.mpp.as_ref()
    }
    fn session_mpp(&self) -> Option<&SessionMpp> {
        self.session_mpp.as_deref()
    }
}

fn load_test_api() -> ApiSpec {
    let content = std::fs::read_to_string("tests/fixtures/test-paywall.yml").unwrap();
    serde_yml::from_str(&content).unwrap()
}

async fn echo_handler(req: Request<Body>) -> impl IntoResponse {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    axum::Json(json!({ "method": method, "path": path, "echo": true }))
}

async fn start_test_server(with_mpp: bool) -> (String, tokio::task::JoinHandle<()>) {
    start_test_server_with(load_test_api(), with_mpp).await
}

async fn start_test_server_with(
    api: ApiSpec,
    with_mpp: bool,
) -> (String, tokio::task::JoinHandle<()>) {
    let mpp = if with_mpp {
        Mpp::new(pay_kit::mpp::server::Config {
            recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
            currency: "SOL".to_string(),
            decimals: 9,
            network: "localnet".to_string(),
            rpc_url: Some("http://localhost:8899".to_string()),
            challenge_binding_secret: Some("test-secret-key-do-not-use-32b-pad".to_string()),
            realm: Some("test-realm".to_string()),
            store: Some(std::sync::Arc::new(pay_kit::mpp::store::MemoryStore::new())),
            ..Default::default()
        })
        .ok()
    } else {
        None
    };

    let state = TestState {
        apis: Arc::new(vec![api]),
        mpp,
    };

    let app = Router::new()
        .route(
            "/__402/health",
            get(|| async { axum::Json(json!({"ok": true})) }),
        )
        .fallback(any(echo_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<TestState>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (url, handle)
}

async fn start_multi_currency_server() -> (String, tokio::task::JoinHandle<()>) {
    let api = load_test_api();
    let mpps = ["USDC", "CASH", "USDT"]
        .into_iter()
        .map(|currency| {
            Mpp::new(pay_kit::mpp::server::Config {
                recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
                currency: currency.to_string(),
                decimals: 6,
                network: "localnet".to_string(),
                rpc_url: Some("http://localhost:8899".to_string()),
                challenge_binding_secret: Some("test-secret-key-do-not-use-32b-pad".to_string()),
                realm: Some("test-realm".to_string()),
                store: Some(std::sync::Arc::new(pay_kit::mpp::store::MemoryStore::new())),
                ..Default::default()
            })
            .unwrap()
        })
        .collect();

    let state = MultiCurrencyTestState {
        apis: Arc::new(vec![api]),
        mpps,
    };

    let app = Router::new()
        .fallback(any(echo_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<MultiCurrencyTestState>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (url, handle)
}

/// Production fallback (start.rs): forward only what the payment gate routed.
async fn gated_forward(api: &ApiSpec, req: axum::http::Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let Some(route) = parts.extensions.get::<proxy::UpstreamRoute>().cloned() else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "not_found"})),
        )
            .into_response();
    };
    let bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
        .await
        .unwrap_or_default();
    proxy::forward_request(api, &route, parts.method, &parts.uri, &parts.headers, bytes)
        .await
        .unwrap_or_else(|e| e)
}

fn load_respond_api() -> ApiSpec {
    let content = std::fs::read_to_string("tests/fixtures/test-respond.yml").unwrap();
    serde_yml::from_str(&content).unwrap()
}

fn client_with_host(subdomain: &str) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert("host", format!("{subdomain}.localhost").parse().unwrap());
    h
}

async fn start_respond_server() -> (String, tokio::task::JoinHandle<()>) {
    let api = load_respond_api();
    let mpp = Mpp::new(pay_kit::mpp::server::Config {
        recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
        currency: "USDC".to_string(),
        decimals: 6,
        network: "localnet".to_string(),
        rpc_url: Some("http://localhost:8899".to_string()),
        challenge_binding_secret: Some("test-secret-key-do-not-use-32b-pad".to_string()),
        realm: Some("test-realm".to_string()),
        store: Some(std::sync::Arc::new(pay_kit::mpp::store::MemoryStore::new())),
        ..Default::default()
    })
    .ok();

    let state = TestState {
        apis: Arc::new(vec![api.clone()]),
        mpp,
    };

    // Use forward_request as fallback (matches production behavior)
    let app = Router::new()
        .fallback(any(move |req: axum::http::Request<Body>| {
            let api = api.clone();
            async move { gated_forward(&api, req).await }
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<TestState>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (url, handle)
}

fn test_session_blockhash_cache() -> pay_kit::mpp::blockhash::BlockhashCache {
    let cache = pay_kit::mpp::blockhash::BlockhashCache::new();
    cache.set(
        "SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxx11x".to_string(),
        42,
        123,
    );
    cache
}

fn test_session_mpp_with_store(
    currency: &str,
) -> (SessionMpp, Arc<dyn pay_kit::mpp::store::ChannelStore>) {
    let store: Arc<dyn pay_kit::mpp::store::ChannelStore> =
        Arc::new(pay_kit::mpp::store::MemoryChannelStore::new());
    let session = SessionMpp::new_with_channel_store(
        SessionConfig {
            operator: solana_pubkey::Pubkey::new_unique().to_string(),
            recipient: solana_pubkey::Pubkey::new_unique().to_string(),
            suggested_deposit: Some(5_000_000),
            currency: currency.to_string(),
            network: "localnet".to_string(),
            ..SessionConfig::default()
        },
        "test-secret-key-do-not-use-32b-pad",
        Arc::clone(&store),
    )
    .with_blockhash_cache(test_session_blockhash_cache());
    (session, store)
}

fn test_session_mpp_for_currency(currency: &str) -> SessionMpp {
    test_session_mpp_with_store(currency).0
}

async fn start_session_server() -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<dyn pay_kit::mpp::store::ChannelStore>,
) {
    start_session_server_with(load_test_api()).await
}

async fn start_session_server_with(
    api: ApiSpec,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<dyn pay_kit::mpp::store::ChannelStore>,
) {
    let (session, store) =
        test_session_mpp_with_store(&solana_pubkey::Pubkey::new_unique().to_string());
    let state = SessionTestState {
        apis: Arc::new(vec![api]),
        mpp: None,
        session_mpp: Some(Arc::new(session)),
    };

    let app = Router::new()
        .fallback(any(echo_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<SessionTestState>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (url, handle, store)
}

async fn start_multi_currency_session_server() -> (String, tokio::task::JoinHandle<()>) {
    use pay_types::stablecoin_mints::{
        CASH_MAINNET, PYUSD_MAINNET, USDC_MAINNET, USDG_MAINNET, USDT_MAINNET,
    };

    let api = load_test_api();
    let session_mpps = [
        USDC_MAINNET,
        USDT_MAINNET,
        PYUSD_MAINNET,
        CASH_MAINNET,
        USDG_MAINNET,
    ]
    .into_iter()
    .map(|currency| Arc::new(test_session_mpp_for_currency(currency)))
    .collect();
    let state = MultiCurrencySessionTestState {
        apis: Arc::new(vec![api]),
        session_mpps,
    };

    let app = Router::new()
        .fallback(any(echo_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<MultiCurrencySessionTestState>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (url, handle)
}

// =============================================================================
// proxy::resolve_api
// =============================================================================

#[test]
fn resolve_api_by_subdomain() {
    let api = load_test_api();
    let apis = vec![api];
    let found = proxy::resolve_api(&apis, "testapi.google.agent-gateway.solana.com");
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "testapi");
}

#[test]
fn resolve_api_unknown() {
    let api = load_test_api();
    let apis = vec![api];
    assert!(proxy::resolve_api(&apis, "unknown.localhost").is_none());
}

#[test]
fn resolve_api_empty_host() {
    let api = load_test_api();
    let apis = vec![api];
    assert!(proxy::resolve_api(&apis, "").is_none());
}

// =============================================================================
// proxy::error_response
// =============================================================================

#[test]
fn error_response_format() {
    let resp = proxy::error_response(StatusCode::NOT_FOUND, "not found");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn error_response_bad_gateway() {
    let resp = proxy::error_response(StatusCode::BAD_GATEWAY, "upstream down");
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

// =============================================================================
// Payment middleware — free endpoints
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_skips_gateway_routes() {
    let (url, _h) = start_test_server(true).await;
    let resp = reqwest::get(format!("{url}/__402/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_passes_free_endpoints() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/v1/health"))
        .headers(client_with_host("testapi"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["echo"], true);
}

#[ignore = "host/localhost passthrough is environment-sensitive and not payment-critical"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_passes_unknown_subdomain() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/anything"))
        .headers(client_with_host("unknown"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_uses_single_api_mode_without_host_header() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    assert!(resp.headers().get("www-authenticate").is_some());
}

// =============================================================================
// Payment middleware — 402 responses
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_returns_402_for_metered() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    assert!(resp.headers().get("www-authenticate").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_402_body_has_pricing() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "payment_required");
    assert!(body["endpoint"]["path"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_402_challenge_parseable() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    let challenge = pay_kit::mpp::parse_www_authenticate(www_auth).unwrap();
    assert_eq!(challenge.method.as_str(), "solana");
    assert_eq!(challenge.intent.as_str(), "charge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_returns_one_challenge_per_configured_currency() {
    let (url, _h) = start_multi_currency_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 402);
    let headers: Vec<_> = resp
        .headers()
        .get_all("www-authenticate")
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    let challenges = pay_kit::mpp::parse_www_authenticate_all(headers)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let currencies: Vec<String> = challenges
        .into_iter()
        .map(|challenge| {
            let request: pay_kit::mpp::ChargeRequest = challenge.request.decode().unwrap();
            request.currency
        })
        .collect();
    assert_eq!(currencies, ["USDC", "CASH", "USDT"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_html_payment_link_sets_html_content_type_and_challenge() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .header("accept", "text/html,*/*")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 402);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("text/html"));
    assert!(resp.headers().get("www-authenticate").is_some());
}

// =============================================================================
// Payment middleware — invalid credentials
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_rejects_garbage_auth() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .header("authorization", "Payment dGhpcyBpcyBnYXJiYWdl")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "malformed_credential");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_rejects_bearer_scheme() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .header("authorization", "Bearer some-token")
        .body("{}")
        .send()
        .await
        .unwrap();
    // Bearer is not "Payment" scheme — should be rejected
    assert!(resp.status() == 400 || resp.status() == 402);
}

// =============================================================================
// Payment middleware — session flow
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_returns_session_challenge_when_session_mpp_configured() {
    let (url, _h, _store) = start_session_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 402);
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    let challenge = pay_kit::mpp::parse_www_authenticate(www_auth).unwrap();
    assert_eq!(challenge.intent.as_str(), "session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_returns_one_session_challenge_per_configured_currency() {
    use pay_types::stablecoin_mints::{
        CASH_MAINNET, PYUSD_MAINNET, USDC_MAINNET, USDG_MAINNET, USDT_MAINNET,
    };

    let (url, _h) = start_multi_currency_session_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 402);
    let headers: Vec<_> = resp
        .headers()
        .get_all("www-authenticate")
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    let challenges = pay_kit::mpp::parse_www_authenticate_all(headers)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let currencies: Vec<String> = challenges
        .into_iter()
        .map(|challenge| {
            assert_eq!(challenge.intent.as_str(), "session");
            let request: pay_kit::mpp::SessionRequest = challenge.request.decode().unwrap();
            request.currency
        })
        .collect();
    assert_eq!(
        currencies,
        [
            USDC_MAINNET,
            USDT_MAINNET,
            PYUSD_MAINNET,
            CASH_MAINNET,
            USDG_MAINNET,
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_accepts_session_voucher_then_close() {
    let (url, _h, store) = start_session_server().await;
    let client = reqwest::Client::new();

    let challenge_resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let www_auth = challenge_resp
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let challenge = pay_kit::mpp::parse_www_authenticate(&www_auth).unwrap();

    // Opens now verify, broadcast, and confirm a real transaction (covered by
    // the surfpool e2e tests); seed the confirmed channel state directly and
    // drive the middleware with vouchers and close.
    let channel = solana_pubkey::Pubkey::new_unique();
    let (voucher_key, signer) = {
        use ed25519_dalek::SigningKey;
        let sk = SigningKey::generate(&mut rand::thread_rng());
        let vk = sk.verifying_key();
        let mut kp = [0u8; 64];
        kp[..32].copy_from_slice(sk.as_bytes());
        kp[32..].copy_from_slice(vk.as_bytes());
        let signer: Box<dyn pay_kit::mpp::solana_keychain::TransactionSigner> =
            Box::new(pay_kit::mpp::solana_keychain::memory::MemorySigner::from_bytes(&kp).unwrap());
        (sk, signer)
    };
    let authorized_signer = signer.pubkey().to_string();
    store
        .put_channel(
            &channel.to_string(),
            pay_core::server::session::test_channel_state(
                channel.to_string(),
                1_000_000,
                &authorized_signer,
                "client",
                &challenge.id,
                solana_pubkey::Pubkey::new_unique().to_string(),
                None,
            ),
        )
        .await
        .unwrap();
    let handle = pay_core::session::SessionHandle::new(channel, signer, challenge.clone())
        .with_voucher_key(voucher_key.clone());
    let price: u64 = challenge
        .request
        .decode::<pay_kit::mpp::SessionRequest>()
        .unwrap()
        .amount
        .parse()
        .unwrap();

    // A voucher below the challenged route price is refused before upstream.
    let under = pay_core::session::SessionHandle::new(
        channel,
        Box::new(
            pay_kit::mpp::solana_keychain::memory::MemorySigner::from_bytes(&{
                let mut kp = [0u8; 64];
                kp[..32].copy_from_slice(voucher_key.as_bytes());
                kp[32..].copy_from_slice(voucher_key.verifying_key().as_bytes());
                kp
            })
            .unwrap(),
        ),
        challenge.clone(),
    );
    let under_resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .header(
            "authorization",
            under.voucher_header(price - 1).await.unwrap(),
        )
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(under_resp.status(), 402);

    let voucher_header = handle.voucher_header(price).await.unwrap();
    let voucher_resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .header("authorization", &voucher_header)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(voucher_resp.status(), 200);
    handle.record_voucher(&voucher_header).await.unwrap();

    let close_header = handle.close_header(Some(price)).await.unwrap();
    let close_resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .header("authorization", close_header)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(close_resp.status(), 200);
    let body: serde_json::Value = close_resp.json().await.unwrap();
    assert_eq!(body["status"], "closed");
}

/// Seed a confirmed client-signed channel for `challenge` and return a handle
/// that signs its vouchers.
async fn seed_client_session(
    store: &Arc<dyn pay_kit::mpp::store::ChannelStore>,
    challenge: &pay_kit::mpp::PaymentChallenge,
) -> pay_core::session::SessionHandle {
    let channel = solana_pubkey::Pubkey::new_unique();
    let key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
    let mut kp = [0u8; 64];
    kp[..32].copy_from_slice(key.as_bytes());
    kp[32..].copy_from_slice(key.verifying_key().as_bytes());
    let signer: Box<dyn pay_kit::mpp::solana_keychain::TransactionSigner> =
        Box::new(pay_kit::mpp::solana_keychain::memory::MemorySigner::from_bytes(&kp).unwrap());
    store
        .put_channel(
            &channel.to_string(),
            pay_core::server::session::test_channel_state(
                channel.to_string(),
                1_000_000,
                signer.pubkey().to_string(),
                "client",
                &challenge.id,
                solana_pubkey::Pubkey::new_unique().to_string(),
                None,
            ),
        )
        .await
        .unwrap();
    pay_core::session::SessionHandle::new(channel, signer, challenge.clone()).with_voucher_key(key)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_never_forwards_a_replayed_session_voucher() {
    let (url, _h, store) = start_session_server().await;
    let client = reqwest::Client::new();
    let send = |authorization: Option<String>| {
        let mut request = client
            .post(format!("{url}/v1/simple/echo"))
            .headers(client_with_host("testapi"))
            .body("{}");
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        request.send()
    };
    let challenge_resp = send(None).await.unwrap();
    let challenge = pay_kit::mpp::parse_www_authenticate(
        challenge_resp
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let price: u64 = challenge
        .request
        .decode::<pay_kit::mpp::SessionRequest>()
        .unwrap()
        .amount
        .parse()
        .unwrap();
    let handle = seed_client_session(&store, &challenge).await;

    let voucher = handle.voucher_header(price).await.unwrap();
    assert_eq!(send(Some(voucher.clone())).await.unwrap().status(), 200);
    handle.record_voucher(&voucher).await.unwrap();
    for _ in 0..2 {
        let replay = send(Some(voucher.clone())).await.unwrap();
        assert_eq!(
            replay.status(),
            402,
            "an exact replay must not reach upstream"
        );
        let body: serde_json::Value = replay.json().await.unwrap();
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|message| message.contains("already accepted")),
            "{body}"
        );
    }

    // The next request is paid by a fresh voucher.
    let next = handle.voucher_header(price).await.unwrap();
    assert_eq!(send(Some(next)).await.unwrap().status(), 200);
}

async fn session_challenge_amount(
    client: &reqwest::Client,
    url: &str,
    path: &str,
) -> (pay_kit::mpp::PaymentChallenge, u64) {
    let resp = client
        .post(format!("{url}/{path}"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let challenge = pay_kit::mpp::parse_www_authenticate(
        resp.headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let amount = challenge
        .request
        .decode::<pay_kit::mpp::SessionRequest>()
        .unwrap()
        .amount
        .parse()
        .unwrap();
    (challenge, amount)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_binds_session_vouchers_to_the_route_price() {
    // A premium copy of the flat echo route, priced 500x higher.
    let mut api = load_test_api();
    let mut premium = api
        .endpoints
        .iter()
        .find(|endpoint| endpoint.path == "v1/simple/echo")
        .unwrap()
        .clone();
    premium.path = "v1/simple/premium".to_string();
    premium.metering.as_mut().unwrap().dimensions[0].tiers[0].price_usd = 0.5;
    api.endpoints.push(premium);
    let (url, _h, store) = start_session_server_with(api).await;
    let client = reqwest::Client::new();
    let (cheap, cheap_price) = session_challenge_amount(&client, &url, "v1/simple/echo").await;
    let (expensive, expensive_price) =
        session_challenge_amount(&client, &url, "v1/simple/premium").await;
    assert_eq!((cheap_price, expensive_price), (1_000, 500_000));

    let post = |path: &str, authorization: String| {
        client
            .post(format!("{url}/{path}"))
            .headers(client_with_host("testapi"))
            .header("authorization", authorization)
            .body("{}")
            .send()
    };
    // A voucher answering the cheap challenge cannot pay for the premium route.
    let handle = seed_client_session(&store, &cheap).await;
    let resp = post(
        "v1/simple/premium",
        handle.voucher_header(cheap_price).await.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 402);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("issued at 1000 base units")),
        "{body}"
    );
    // Each route still accepts a voucher answering its own challenge.
    let handle = seed_client_session(&store, &cheap).await;
    let resp = post(
        "v1/simple/echo",
        handle.voucher_header(cheap_price).await.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let handle = seed_client_session(&store, &expensive).await;
    let resp = post(
        "v1/simple/premium",
        handle.voucher_header(expensive_price).await.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_binds_charge_credentials_to_their_route() {
    // A second route with exactly the same payment terms as the echo route.
    let mut api = load_test_api();
    let mut twin = api
        .endpoints
        .iter()
        .find(|endpoint| endpoint.path == "v1/simple/echo")
        .unwrap()
        .clone();
    twin.path = "v1/simple/twin".to_string();
    api.endpoints.push(twin);
    let (url, _h) = start_test_server_with(api, true).await;
    let client = reqwest::Client::new();
    let charge_challenge = |path: &'static str| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let resp = client
                .post(format!("{url}/{path}"))
                .headers(client_with_host("testapi"))
                .body("{}")
                .send()
                .await
                .unwrap();
            let headers: Vec<_> = resp
                .headers()
                .get_all("www-authenticate")
                .iter()
                .map(|value| value.to_str().unwrap().to_string())
                .collect();
            pay_kit::mpp::parse_www_authenticate_all(headers.iter().map(String::as_str))
                .into_iter()
                .filter_map(Result::ok)
                .find(|challenge| challenge.intent.as_str() == "charge")
                .expect("charge challenge")
        }
    };
    let present = |path: &'static str, challenge: pay_kit::mpp::PaymentChallenge| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let credential = pay_kit::mpp::PaymentCredential::new(
                challenge.to_echo(),
                json!({ "type": "transaction", "transaction": "AQAB" }),
            );
            let resp = client
                .post(format!("{url}/{path}"))
                .headers(client_with_host("testapi"))
                .header(
                    "authorization",
                    pay_kit::mpp::format_authorization(&credential).unwrap(),
                )
                .body("{}")
                .send()
                .await
                .unwrap();
            (resp.status(), resp.text().await.unwrap())
        }
    };

    let echo = charge_challenge("v1/simple/echo").await;
    let (status, body) = present("v1/simple/twin", echo.clone()).await;
    assert_eq!(status, 402);
    assert!(body.contains("different route"), "{body}");
    // On its own route the credential passes the route check and is judged
    // on its (here invalid) payment instead.
    let (status, body) = present("v1/simple/echo", echo).await;
    assert_eq!(status, 402);
    assert!(!body.contains("different route"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_garbage_auth_includes_message() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .header("authorization", "Payment dGhpcyBpcyBnYXJiYWdl")
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["message"].is_string());
}

// =============================================================================
// Payment middleware — no MPP configured
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_fails_closed_without_mpp() {
    let (url, _h) = start_test_server(false).await;
    let client = reqwest::Client::new();
    // Metered endpoint but no MPP configured → fail closed (issue #373).
    // Forwarding here would serve a paid resource for free, so the
    // middleware must reject rather than pass through.
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        500,
        "metered endpoint with no payment backend must fail closed, not pass through"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "payment_backend_unconfigured");
}

// =============================================================================
// Accounting store — additional edge cases
// =============================================================================

#[test]
fn accounting_zero_increment() {
    let store = InMemoryStore::new();
    let key = AccountingKey {
        api: "test".into(),
        endpoint: "test".into(),
        period: "2026-03".into(),
        scope: "pool".into(),
    };
    assert_eq!(store.increment(&key, 0), 0);
    assert_eq!(store.get_usage(&key), 0);
}

#[test]
fn accounting_large_increment() {
    let store = InMemoryStore::new();
    let key = AccountingKey {
        api: "test".into(),
        endpoint: "test".into(),
        period: "2026-03".into(),
        scope: "pool".into(),
    };
    assert_eq!(store.increment(&key, u64::MAX / 2), u64::MAX / 2);
}

#[test]
fn accounting_many_scopes() {
    let store = InMemoryStore::new();
    for i in 0..100 {
        let key = AccountingKey {
            api: "test".into(),
            endpoint: "test".into(),
            period: "2026-03".into(),
            scope: format!("wallet_{i}"),
        };
        store.increment(&key, i as u64);
    }
    let key = AccountingKey {
        api: "test".into(),
        endpoint: "test".into(),
        period: "2026-03".into(),
        scope: "wallet_50".into(),
    };
    assert_eq!(store.get_usage(&key), 50);
}

// =============================================================================
// Root redirect to PDB (when debugger is active)
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_redirects_to_pdb_with_html_accept() {
    // Simulates `pay gate api`: root with Accept:text/html → redirect to pdb.
    // Unlisted paths (e.g. /favicon.ico) must return 404, not forward to upstream
    // (which would trigger spurious OAuth2 fetches).
    let api = load_respond_api();
    let mpp = Mpp::new(pay_kit::mpp::server::Config {
        recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
        currency: "USDC".to_string(),
        decimals: 6,
        network: "localnet".to_string(),
        rpc_url: Some("http://localhost:8899".to_string()),
        challenge_binding_secret: Some("test-secret-key-do-not-use-32b-pad".to_string()),
        realm: Some("test-realm".to_string()),
        store: Some(std::sync::Arc::new(pay_kit::mpp::store::MemoryStore::new())),
        ..Default::default()
    })
    .ok();

    let state = TestState {
        apis: Arc::new(vec![api.clone()]),
        mpp,
    };

    let app = Router::new()
        .route(
            "/",
            get(|headers: axum::http::HeaderMap| async move {
                let accepts_html = headers
                    .get("accept")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.contains("text/html"));
                if accepts_html {
                    axum::response::Redirect::temporary("/__402/pdb/").into_response()
                } else {
                    axum::Json(json!({"status": "ok"})).into_response()
                }
            }),
        )
        .fallback(any(move |req: axum::http::Request<Body>| {
            let api = api.clone();
            async move { gated_forward(&api, req).await }
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<TestState>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let _h = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // HTML accept → redirect to PDB.
    let resp = client
        .get(&url)
        .header("accept", "text/html,*/*")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 307);
    assert_eq!(resp.headers().get("location").unwrap(), "/__402/pdb/");

    // JSON accept → 200 status ok.
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // Unlisted path → 404, not a proxy attempt.
    let resp = client
        .get(format!("{url}/favicon.ico"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// =============================================================================
// Method gating — prevent bypass by switching HTTP methods
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_on_post_endpoint_returns_402_with_html_accept() {
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .header("accept", "text/html,*/*")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        402,
        "GET with Accept:text/html on POST endpoint should return 402 payment link"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_on_post_endpoint_without_html_passes_through() {
    // With proxy routing, unknown method falls through to upstream (echo handler)
    let (url, _h) = start_test_server(true).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .send()
        .await
        .unwrap();
    // Proxy routing: passes through to fallback (echo handler returns 200)
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_on_get_endpoint_returns_402() {
    // Uses respond server which has a metered GET endpoint
    let (url, _h) = start_respond_server().await;
    let client = reqwest::Client::new();
    let resp = client.head(format!("{url}/v1/data")).send().await.unwrap();
    // HEAD should be gated same as GET
    assert_eq!(resp.status(), 402);
}

// =============================================================================
// Respond routing — method gating and 404 behavior
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respond_get_metered_returns_402() {
    let (url, _h) = start_respond_server().await;
    let client = reqwest::Client::new();
    let resp = client.get(format!("{url}/v1/data")).send().await.unwrap();
    assert_eq!(
        resp.status(),
        402,
        "GET on metered respond endpoint should return 402"
    );
    assert!(resp.headers().get("www-authenticate").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respond_post_metered_returns_402() {
    let (url, _h) = start_respond_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/submit"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        402,
        "POST on metered respond endpoint should return 402"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respond_free_endpoint_passes_through() {
    let (url, _h) = start_respond_server().await;
    let client = reqwest::Client::new();
    let resp = client.get(format!("{url}/v1/health")).send().await.unwrap();
    // Free endpoint with respond routing: passes to fallback (echo)
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respond_unknown_path_returns_404() {
    let (url, _h) = start_respond_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/v1/nonexistent"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "Unknown path on respond routing should return 404"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respond_wrong_method_returns_404() {
    let (url, _h) = start_respond_server().await;
    let client = reqwest::Client::new();
    // GET on a POST-only endpoint without Accept:text/html
    let resp = client.get(format!("{url}/v1/submit")).send().await.unwrap();
    assert_eq!(
        resp.status(),
        404,
        "GET on POST endpoint with respond routing should return 404"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respond_wrong_method_with_html_returns_402() {
    let (url, _h) = start_respond_server().await;
    let client = reqwest::Client::new();
    // GET on POST endpoint with Accept:text/html → payment link page
    let resp = client
        .get(format!("{url}/v1/submit"))
        .header("accept", "text/html,*/*")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        402,
        "GET with Accept:text/html on POST endpoint should return 402 payment link"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respond_service_worker_always_served() {
    let (url, _h) = start_respond_server().await;
    let client = reqwest::Client::new();
    // Service worker request on a POST endpoint path
    let resp = client
        .get(format!("{url}/v1/submit?__mpp_worker=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("javascript"), "Service worker should return JS");
}

// ── Delegated buffered response settlement ──

mod delegated_buffered_rounding {
    use super::*;
    use axum::Json;
    use pay_core::server::session::test_channel_state;
    use pay_core::session::SessionHandle;
    use pay_kit::core::store::{ChannelStore, MemoryChannelStore};
    use pay_kit::mpp::SessionAuthentication;
    use pay_kit::mpp::server::session::VoucherSigner;
    use pay_kit::solana_keychain::{MemorySigner, TransactionSigner};

    #[derive(Clone)]
    struct DelegatedState {
        apis: Arc<Vec<ApiSpec>>,
        session: Arc<SessionMpp>,
    }

    impl PaymentState for DelegatedState {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&Mpp> {
            None
        }
        fn session_mpp(&self) -> Option<&SessionMpp> {
            Some(&self.session)
        }
        fn session_mpp_handle(&self) -> Option<Arc<SessionMpp>> {
            Some(self.session.clone())
        }
    }

    fn operator_keypair() -> [u8; 64] {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[41; 32]);
        let mut keypair = [0_u8; 64];
        keypair[..32].copy_from_slice(signing_key.as_bytes());
        keypair[32..].copy_from_slice(signing_key.verifying_key().as_bytes());
        keypair
    }

    fn api_spec() -> ApiSpec {
        serde_yml::from_str(
            r#"
name: delegated-rounding
subdomain: rounding
title: "Delegated buffered rounding"
description: "A response-metered delegated session endpoint."
category: ai_ml
version: v1
routing:
  type: respond
endpoints:
  - method: POST
    path: v1/chat
    metering:
      schemes: [mpp-session]
      dimensions:
        - direction: input
          unit: tokens
          scale: 1000000
          tiers:
            - price_usd: 0.10
          meter:
            source: response_json
            path: /usage/prompt_tokens
        - direction: output
          unit: tokens
          scale: 1000000
          tiers:
            - price_usd: 0.30
          meter:
            source: response_json
            path: /usage/completion_tokens
      upto:
        max_usd: 0.02
        missing_usage: refund
        usage_preset: openai-compatible
"#,
        )
        .unwrap()
    }

    async fn cumulative_after_requests(prompt_tokens: u64, completion_tokens: u64) -> Vec<u64> {
        let operator: Arc<dyn TransactionSigner> =
            Arc::new(MemorySigner::from_bytes(&operator_keypair()).unwrap());
        let store: Arc<dyn ChannelStore> = Arc::new(MemoryChannelStore::new());
        let session = Arc::new(
            SessionMpp::new_with_channel_store(
                SessionConfig {
                    operator: operator.pubkey().to_string(),
                    recipient: operator.pubkey().to_string(),
                    suggested_deposit: Some(1_000_000),
                    currency: solana_pubkey::Pubkey::new_unique().to_string(),
                    network: "localnet".to_string(),
                    decimals: 6,
                    voucher_signer: VoucherSigner::Operator,
                    ..SessionConfig::default()
                },
                "delegated-buffered-rounding-secret",
                store.clone(),
            )
            .with_blockhash_cache({
                let cache = pay_kit::mpp::blockhash::BlockhashCache::new();
                cache.set("SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxx11x".into(), 42, 123);
                cache
            })
            .with_payment_channel_signer(operator.clone()),
        );
        let state = DelegatedState {
            apis: Arc::new(vec![api_spec()]),
            session: session.clone(),
        };
        let app = Router::new()
            .fallback(any(move || async move {
                (
                    StatusCode::OK,
                    Json(json!({
                        "usage": {
                            "prompt_tokens": prompt_tokens,
                            "completion_tokens": completion_tokens
                        },
                        "choices": []
                    })),
                )
            }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                pay_core::server::payment::payment_middleware::<DelegatedState>,
            ))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/chat", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let host = "rounding.localhost";

        let challenge_response = client
            .post(&url)
            .header("host", host)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(challenge_response.status(), StatusCode::PAYMENT_REQUIRED);
        let challenge = pay_kit::mpp::parse_www_authenticate(
            challenge_response
                .headers()
                .get("www-authenticate")
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();

        let channel = solana_pubkey::Pubkey::new_unique();
        let payer = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let proof = SessionAuthentication::sign(challenge.id.clone(), &channel.to_string(), &payer)
            .unwrap();
        store
            .put_channel(
                &channel.to_string(),
                test_channel_state(
                    channel.to_string(),
                    1_000_000,
                    operator.pubkey().to_string(),
                    "operator",
                    challenge.id.clone(),
                    proof.payer.clone(),
                    Some(serde_json::to_string(&proof).unwrap()),
                ),
            )
            .await
            .unwrap();
        let handle = SessionHandle::new(
            channel,
            Box::new(MemorySigner::from_bytes(&operator_keypair()).unwrap()),
            challenge,
        )
        .with_authentication(proof);

        let mut cumulative = Vec::new();
        for _ in 0..2 {
            let response = client
                .post(&url)
                .header("host", host)
                .header("authorization", handle.use_header().await.unwrap())
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            cumulative.push(
                store
                    .get_channel(&channel.to_string())
                    .await
                    .unwrap()
                    .unwrap()
                    .cumulative,
            );
        }
        server.abort();
        cumulative
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sub_base_unit_usage_advances_the_delegated_voucher() {
        assert_eq!(cumulative_after_requests(1, 1).await, vec![1, 2]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn whole_base_unit_usage_settles_exactly() {
        assert_eq!(
            cumulative_after_requests(1_000, 1_000).await,
            vec![400, 800]
        );
    }
}

// ── Static delegated sessions charge completed redirects ──

mod static_session_redirects {
    use super::*;
    use pay_core::server::payment::payment_middleware;
    use pay_core::server::session::test_channel_state;
    use pay_kit::mpp::SessionAuthentication;
    use pay_kit::mpp::server::session::VoucherSigner;
    use pay_kit::mpp::store::{ChannelStore, MemoryChannelStore};
    use pay_kit::solana_keychain::{MemorySigner, SolanaSigner, TransactionSigner};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[derive(Clone)]
    struct StaticState {
        apis: Arc<Vec<ApiSpec>>,
        session: Arc<SessionMpp>,
    }

    impl PaymentState for StaticState {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&Mpp> {
            None
        }
        fn session_mpp(&self) -> Option<&SessionMpp> {
            Some(&self.session)
        }
        fn session_mpp_handle(&self) -> Option<Arc<SessionMpp>> {
            Some(self.session.clone())
        }
    }

    fn signer() -> MemorySigner {
        MemorySigner::from_bytes(&solana_keypair::Keypair::new().to_bytes()).unwrap()
    }

    /// Spent amount after two paid requests whose handler returns `status`.
    async fn spent_after_two_requests(status: StatusCode) -> (u64, usize) {
        let operator = signer();
        let operator_address = operator.pubkey().to_string();
        let operator: Arc<dyn TransactionSigner> = Arc::new(operator);
        let store = Arc::new(MemoryChannelStore::new());
        let config = SessionConfig {
            operator: operator_address.clone(),
            recipient: solana_pubkey::Pubkey::new_unique().to_string(),
            currency: "USDC".to_string(),
            decimals: 6,
            network: "localnet".to_string(),
            voucher_signer: VoucherSigner::Operator,
            ..SessionConfig::default()
        };
        let session = Arc::new(
            SessionMpp::new_with_channel_store(config, "static-redirect-secret", store.clone())
                .with_blockhash_cache({
                    let cache = pay_kit::mpp::blockhash::BlockhashCache::new();
                    cache.set("SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxx11x".into(), 42, 123);
                    cache
                })
                .with_payment_channel_signer(operator),
        );
        let challenge = session.challenge(None).unwrap();
        let channel = solana_pubkey::Pubkey::new_unique();
        let payer = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let proof = SessionAuthentication::sign(challenge.id.clone(), &channel.to_string(), &payer)
            .unwrap();
        store
            .put_channel(
                &channel.to_string(),
                test_channel_state(
                    channel.to_string(),
                    1_000_000,
                    operator_address,
                    "operator",
                    &challenge.id,
                    proof.payer.clone(),
                    Some(serde_json::to_string(&proof).unwrap()),
                ),
            )
            .await
            .unwrap();
        let authorization =
            pay_core::client::session::SessionHandle::new(channel, Box::new(signer()), challenge)
                .with_authentication(proof)
                .use_header()
                .await
                .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let app = Router::new()
            .fallback(any(move || {
                handler_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    axum::response::Response::builder()
                        .status(status)
                        .header("location", "/completed")
                        .body(Body::from("completed"))
                        .unwrap()
                }
            }))
            .layer(middleware::from_fn_with_state(
                StaticState {
                    apis: Arc::new(vec![
                        serde_yml::from_str(include_str!("fixtures/test-paywall.yml")).unwrap(),
                    ]),
                    session: session.clone(),
                },
                payment_middleware::<StaticState>,
            ));

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/simple/echo")
                        .header("host", "testapi")
                        .header("authorization", authorization.as_str())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
        let state = store
            .get_channel(&channel.to_string())
            .await
            .unwrap()
            .unwrap();
        (state.spent_amount, calls.load(Ordering::SeqCst))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_redirects_are_charged_like_successes() {
        let (redirect_spent, calls) = spent_after_two_requests(StatusCode::SEE_OTHER).await;
        let (success_spent, _) = spent_after_two_requests(StatusCode::OK).await;
        assert_eq!(calls, 2);
        assert!(success_spent > 0);
        assert_eq!(
            redirect_spent, success_spent,
            "each completed 303 must be charged"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_responses_are_not_charged() {
        let (spent, calls) = spent_after_two_requests(StatusCode::BAD_REQUEST).await;
        assert_eq!(calls, 2);
        assert_eq!(spent, 0);
    }
}

// =============================================================================
// Proxy allowlist: only gate-routed requests reach the upstream
// =============================================================================

type UpstreamHits = Arc<std::sync::Mutex<Vec<(String, bool)>>>;

/// Upstream that records `METHOD /path` and whether operator credentials arrived.
async fn start_recording_upstream() -> (String, UpstreamHits) {
    let hits: UpstreamHits = Arc::default();
    let capture = Arc::clone(&hits);
    let app = Router::new().fallback(any(move |req: Request<Body>| {
        let capture = Arc::clone(&capture);
        async move {
            capture.lock().unwrap().push((
                format!("{} {}", req.method(), req.uri().path()),
                req.headers().contains_key("x-operator-key"),
            ));
            "upstream"
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    (url, hits)
}

/// One declared root `POST ""` (the reported spec) plus a parameterized paid
/// `GET` and a free `GET`, behind operator header auth.
fn allowlist_api(upstream: &str) -> ApiSpec {
    serde_yml::from_str(&format!(
        r#"
name: rpc
subdomain: rpc
title: RPC
description: allowlist
category: ai_ml
version: v1
routing:
  type: proxy
  url: {upstream}
  auth:
    method: header
    key: x-operator-key
    value_from_env: PATH
endpoints:
  - method: POST
    path: ""
    metering:
      schemes: [mpp-session]
      dimensions:
        - direction: usage
          unit: requests
          scale: 1
          tiers:
            - price_usd: 0.001
  - method: GET
    path: "v1/items/{{id}}"
    metering:
      schemes: [mpp-session]
      dimensions:
        - direction: usage
          unit: requests
          scale: 1
          tiers:
            - price_usd: 0.001
  - method: GET
    path: "v1/free"
"#
    ))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_forwards_only_gate_routed_requests_upstream() {
    let (upstream, hits) = start_recording_upstream().await;
    let api = allowlist_api(&upstream);
    let (session, store) =
        test_session_mpp_with_store(&solana_pubkey::Pubkey::new_unique().to_string());
    let state = SessionTestState {
        apis: Arc::new(vec![api.clone()]),
        mpp: None,
        session_mpp: Some(Arc::new(session)),
    };
    let app = Router::new()
        .fallback(any(move |req: Request<Body>| {
            let api = api.clone();
            async move { gated_forward(&api, req).await }
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<SessionTestState>,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let send = |method: reqwest::Method, path: &str, headers: &[(&str, &str)]| {
        let mut request = client.request(method, format!("{url}{path}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send()
    };
    use reqwest::Method as M;

    let challenged = [
        (M::POST, "/", vec![]),
        (M::GET, "/", vec![("accept", "text/html")]),
        (M::GET, "/v1/items/42", vec![]),
        (M::HEAD, "/v1/items/42", vec![]),
    ];
    for (method, path, headers) in challenged {
        let status = send(method.clone(), path, &headers).await.unwrap().status();
        assert_eq!(status, 402, "{method} {path} {headers:?}");
    }
    let undeclared = [
        (M::POST, "/rpc", vec![]),
        (M::POST, "/anything", vec![]),
        (M::GET, "/rpc", vec![]),
        (M::GET, "/", vec![]),
        (M::HEAD, "/", vec![]),
        (M::GET, "/", vec![("authorization", "Payment x")]),
        (M::POST, "/rpc", vec![("authorization", "Payment x")]),
        (M::GET, "/v1/items/42/extra", vec![]),
        (M::DELETE, "/v1/items/42", vec![]),
        (M::POST, "/v1/free", vec![("authorization", "Payment x")]),
    ];
    for (method, path, headers) in undeclared {
        let status = send(method.clone(), path, &headers).await.unwrap().status();
        assert_eq!(status, 404, "{method} {path} {headers:?}");
    }
    assert!(
        hits.lock().unwrap().is_empty(),
        "{:?}",
        hits.lock().unwrap()
    );

    // Positive controls: a free declared route, a paid root POST, and a
    // session commit POSTed to a declared GET resource.
    assert_eq!(send(M::GET, "/v1/free", &[]).await.unwrap().status(), 200);
    for (challenge_method, path) in [(M::POST, "/"), (M::GET, "/v1/items/42")] {
        let www_auth = send(challenge_method, path, &[])
            .await
            .unwrap()
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let challenge = pay_kit::mpp::parse_www_authenticate(&www_auth).unwrap();
        let price: u64 = challenge
            .request
            .decode::<pay_kit::mpp::SessionRequest>()
            .unwrap()
            .amount
            .parse()
            .unwrap();
        let handle = seed_client_session(&store, &challenge).await;
        let voucher = handle.voucher_header(price).await.unwrap();
        let resp = send(M::POST, path, &[("authorization", &voucher)])
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "paid POST {path}");
        // A session credential may not repurpose a free endpoint's method.
        let voucher = handle.voucher_header(price * 2).await.unwrap();
        let resp = send(M::POST, "/v1/free", &[("authorization", &voucher)])
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }
    assert_eq!(
        *hits.lock().unwrap(),
        vec![
            ("GET /v1/free".to_string(), true),
            ("POST /".to_string(), true),
            ("POST /v1/items/42".to_string(), true),
        ]
    );
}

// ── x402 exact is only advertised when it can be settled ──

#[derive(Clone)]
struct X402TestState {
    apis: Arc<Vec<ApiSpec>>,
    mpp: Option<Mpp>,
    x402: Arc<pay_kit::x402::server::X402>,
    signer: Option<Arc<dyn pay_kit::solana_keychain::TransactionSigner>>,
}

impl PaymentState for X402TestState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        self.mpp.as_ref()
    }
    fn x402(&self) -> Option<&pay_kit::x402::server::X402> {
        Some(&self.x402)
    }
    fn fee_payer_signer(&self) -> Option<Arc<dyn pay_kit::solana_keychain::TransactionSigner>> {
        self.signer.clone()
    }
}

fn test_fee_payer() -> Arc<dyn pay_kit::solana_keychain::TransactionSigner> {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let mut keypair = [0u8; 64];
    keypair[..32].copy_from_slice(sk.as_bytes());
    keypair[32..].copy_from_slice(sk.verifying_key().as_bytes());
    Arc::new(pay_kit::solana_keychain::MemorySigner::from_bytes(&keypair).unwrap())
}

async fn start_x402_server(
    fee_payer: Option<Arc<dyn pay_kit::solana_keychain::TransactionSigner>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let mut api = load_test_api();
    for ep in api.endpoints.iter_mut() {
        if ep.path == "v1/simple/echo"
            && let Some(m) = ep.metering.as_mut()
        {
            m.schemes = Some(vec![
                pay_types::metering::Scheme::MppCharge,
                pay_types::metering::Scheme::X402Exact,
            ]);
        }
    }
    let mpp = Mpp::new(pay_kit::mpp::server::Config {
        recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
        currency: "SOL".to_string(),
        decimals: 9,
        network: "localnet".to_string(),
        rpc_url: Some("http://localhost:8899".to_string()),
        challenge_binding_secret: Some("test-secret-key-do-not-use-32b-pad".to_string()),
        realm: Some("test-realm".to_string()),
        store: Some(Arc::new(pay_kit::mpp::store::MemoryStore::new())),
        ..Default::default()
    })
    .ok();
    assert!(mpp.is_some(), "MPP test config must build");
    let x402 = pay_kit::x402::server::X402::new(pay_kit::x402::server::Config {
        recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
        currencies: vec![pay_kit::x402::server::CurrencyConfig {
            currency: "USDC".to_string(),
            decimals: 6,
            token_program: None,
        }],
        network: "devnet".to_string(),
        rpc_url: Some("http://localhost:8899".to_string()),
        resource: "simple".to_string(),
        description: Some("Echo".to_string()),
        max_age: Some(60),
        fee_payer_key: fee_payer.as_ref().map(|s| s.pubkey().to_string()),
    })
    .unwrap();
    let state = X402TestState {
        apis: Arc::new(vec![api]),
        mpp,
        x402: Arc::new(x402),
        signer: fee_payer,
    };
    let app = Router::new()
        .fallback(any(echo_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<X402TestState>,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (url, handle)
}

async fn challenge_headers(url: &str) -> reqwest::header::HeaderMap {
    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    resp.headers().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn x402_exact_advertised_with_fee_payer() {
    let (url, _h) = start_x402_server(Some(test_fee_payer())).await;
    let headers = challenge_headers(&url).await;
    assert!(headers.get("payment-required").is_some());
    assert!(headers.get("www-authenticate").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn x402_exact_withheld_without_fee_payer() {
    let (url, _h) = start_x402_server(None).await;
    let headers = challenge_headers(&url).await;
    assert!(headers.get("payment-required").is_none());
    assert!(headers.get("www-authenticate").is_some());
}
