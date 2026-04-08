//! Tests for server modules: proxy forwarding, payment middleware, accounting.
//!
//! Run: `cargo test -p pay-core --features server --test server_tests`

#![cfg(feature = "server")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use pay_core::PaymentState;
use pay_core::server::accounting::{AccountingKey, AccountingStore, InMemoryStore};
use pay_core::server::proxy;
use pay_types::metering::ApiSpec;
use serde_json::json;
use solana_mpp::server::Mpp;
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

fn load_test_api() -> ApiSpec {
    let content = std::fs::read_to_string("tests/fixtures/test-provider.yml").unwrap();
    serde_yml::from_str(&content).unwrap()
}

async fn echo_handler(req: Request<Body>) -> impl IntoResponse {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    axum::Json(json!({ "method": method, "path": path, "echo": true }))
}

async fn start_test_server(with_mpp: bool) -> (String, tokio::task::JoinHandle<()>) {
    let api = load_test_api();
    let mpp = if with_mpp {
        Mpp::new(solana_mpp::server::Config {
            recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
            currency: "SOL".to_string(),
            decimals: 9,
            network: "localnet".to_string(),
            rpc_url: Some("http://localhost:8899".to_string()),
            secret_key: Some("test-secret".to_string()),
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
    let mpp = Mpp::new(solana_mpp::server::Config {
        recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
        currency: "USDC".to_string(),
        decimals: 6,
        network: "localnet".to_string(),
        rpc_url: Some("http://localhost:8899".to_string()),
        secret_key: Some("test-secret".to_string()),
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
            async move {
                let (parts, body) = req.into_parts();
                let bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                proxy::forward_request(&api, parts.method, &parts.uri, &parts.headers, bytes)
                    .await
                    .unwrap_or_else(|e| e)
            }
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
    let challenge = solana_mpp::parse_www_authenticate(www_auth).unwrap();
    assert_eq!(challenge.method.as_str(), "solana");
    assert_eq!(challenge.intent.as_str(), "charge");
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
// Payment middleware — no MPP configured
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_passes_through_without_mpp() {
    let (url, _h) = start_test_server(false).await;
    let client = reqwest::Client::new();
    // Metered endpoint but no MPP → pass through
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .headers(client_with_host("testapi"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
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
    // Simulates `pay server start` with debugger: root with Accept:text/html → redirect
    let api = load_respond_api();
    let mpp = Mpp::new(solana_mpp::server::Config {
        recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".to_string(),
        currency: "USDC".to_string(),
        decimals: 6,
        network: "localnet".to_string(),
        rpc_url: Some("http://localhost:8899".to_string()),
        secret_key: Some("test-secret".to_string()),
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
            async move {
                let (parts, body) = req.into_parts();
                let bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                proxy::forward_request(&api, parts.method, &parts.uri, &parts.headers, bytes)
                    .await
                    .unwrap_or_else(|e| e)
            }
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

    // HTML accept → redirect to PDB
    let resp = client
        .get(&url)
        .header("accept", "text/html,*/*")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 307);
    assert_eq!(resp.headers().get("location").unwrap(), "/__402/pdb/");

    // JSON accept → 200 status
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
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
