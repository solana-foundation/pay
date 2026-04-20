//! Integration tests using surfpool-sdk (embedded Solana validator).
//!
//! Tests the client modules (balance, send, dev) and server modules
//! (payment middleware) against a real Solana runtime — no external
//! process needed.
//!
//! Run: `cargo test -p pay-core --features server --test surfpool_tests`

#![cfg(feature = "server")]

use pay_core::client;
use std::sync::{Arc, Mutex};
use surfpool_sdk::{Keypair, Signer, Surfnet};

type BatchLog = Arc<Mutex<Vec<Vec<(String, String, u64)>>>>;

// =============================================================================
// Helpers
// =============================================================================

async fn start_surfnet() -> Surfnet {
    Surfnet::builder()
        .offline(true)
        .airdrop_sol(10_000_000_000)
        .start()
        .await
        .expect("Failed to start Surfnet")
}

fn keypair_to_file(keypair: &Keypair) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    let bytes: Vec<u8> = keypair.to_bytes().to_vec();
    write!(file, "{}", serde_json::to_string(&bytes).unwrap()).unwrap();
    file
}

// =============================================================================
// balance
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn balance_funded_account() {
    let surfnet = start_surfnet().await;
    let payer = surfnet.payer();
    let pubkey = payer.pubkey().to_string();

    let rpc = surfnet.rpc_url().to_string();
    let pk = pubkey.clone();
    let balances = client::balance::get_balances(&rpc, &pk).await.unwrap();
    assert!(
        balances.sol_lamports >= 10_000_000_000,
        "Expected >= 10 SOL, got {}",
        balances.sol_lamports
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn balance_empty_account() {
    let surfnet = start_surfnet().await;
    let empty = Keypair::new();

    let rpc = surfnet.rpc_url().to_string();
    let pk = empty.pubkey().to_string();
    let balances = client::balance::get_balances(&rpc, &pk).await.unwrap();
    assert_eq!(balances.sol_lamports, 0);
    assert!(balances.tokens.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn balance_diff_received() {
    let surfnet = start_surfnet().await;
    let payer = surfnet.payer();
    let pubkey = payer.pubkey().to_string();

    let rpc = surfnet.rpc_url().to_string();
    let pk = pubkey.clone();
    let before = client::balance::get_balances(&rpc, &pk).await.unwrap();

    // Fund more SOL
    surfnet
        .cheatcodes()
        .fund_sol(&payer.pubkey(), 15_000_000_000)
        .unwrap();

    let after = client::balance::get_balances(&rpc, &pk).await.unwrap();
    let diff = after.diff_received(&before);
    assert!(diff.sol_lamports > 0, "Should have received more SOL");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn balance_invalid_pubkey() {
    let surfnet = start_surfnet().await;
    let rpc = surfnet.rpc_url().to_string();
    let result = client::balance::get_balances(&rpc, "not-a-pubkey").await;
    assert!(result.is_err());
}

// =============================================================================
// send
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_sol_basic() {
    let surfnet = start_surfnet().await;
    let payer = surfnet.payer();
    let recipient = Keypair::new();

    // Write payer keypair to a temp file
    let kp_file = keypair_to_file(payer);
    let kp_path = kp_file.path().to_string_lossy().to_string();

    let rpc = surfnet.rpc_url().to_string();
    let recip = recipient.pubkey().to_string();
    let kp = kp_path.clone();
    let result = client::send::send_sol("0.5", &recip, &kp, &rpc).await;

    assert!(result.is_ok(), "send_sol failed: {:?}", result.err());
    let result = result.unwrap();
    assert_eq!(result.lamports, 500_000_000);
    assert!(!result.signature.is_empty());

    // Verify recipient got the SOL
    let rpc2 = surfnet.rpc_url().to_string();
    let rpk = recipient.pubkey().to_string();
    let balance = client::balance::get_balances(&rpc2, &rpk).await.unwrap();
    assert_eq!(balance.sol_lamports, 500_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_sol_drain() {
    let surfnet = start_surfnet().await;
    let payer = surfnet.payer();
    let recipient = Keypair::new();

    let kp_file = keypair_to_file(payer);
    let kp_path = kp_file.path().to_string_lossy().to_string();

    // "*" means drain all (minus fees)
    let rpc = surfnet.rpc_url().to_string();
    let recip = recipient.pubkey().to_string();
    let kp = kp_path.clone();
    let result = client::send::send_sol("*", &recip, &kp, &rpc).await;

    assert!(result.is_ok(), "drain failed: {:?}", result.err());

    // Payer should have ~0 SOL left
    let rpc2 = surfnet.rpc_url().to_string();
    let ppk = payer.pubkey().to_string();
    let payer_balance = client::balance::get_balances(&rpc2, &ppk).await.unwrap();
    assert!(
        payer_balance.sol_lamports < 10_000,
        "Payer should be drained, got {}",
        payer_balance.sol_lamports
    );

    // Recipient should have almost all the SOL
    let rpc3 = surfnet.rpc_url().to_string();
    let rpk2 = recipient.pubkey().to_string();
    let recv_balance = client::balance::get_balances(&rpc3, &rpk2).await.unwrap();
    assert!(
        recv_balance.sol_lamports > 9_000_000_000,
        "Recipient should have most SOL"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_sol_invalid_recipient() {
    let surfnet = start_surfnet().await;
    let payer = surfnet.payer();
    let kp_file = keypair_to_file(payer);
    let kp_path = kp_file.path().to_string_lossy().to_string();

    let _rpc = surfnet.rpc_url().to_string();
    let _kp = kp_path.clone();
    let result =
        client::send::send_sol("0.1", "not-a-valid-pubkey", &kp_path, surfnet.rpc_url()).await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_sol_insufficient_funds() {
    let surfnet = start_surfnet().await;
    // Create a wallet with very little SOL
    let broke = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&broke.pubkey(), 5000)
        .unwrap(); // 5000 lamports, not even enough for fees
    let recipient = Keypair::new();

    let kp_file = keypair_to_file(&broke);
    let kp_path = kp_file.path().to_string_lossy().to_string();

    let rpc = surfnet.rpc_url().to_string();
    let recip = recipient.pubkey().to_string();
    let kp = kp_path.clone();
    let result = client::send::send_sol("1.0", &recip, &kp, &rpc).await;
    assert!(result.is_err());
}

// =============================================================================
// dev
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sandbox_setup_keypair() {
    let surfnet = start_surfnet().await;

    let rpc = surfnet.rpc_url().to_string();
    let kp = client::sandbox::setup_sandbox_keypair(&rpc).await;
    assert!(kp.is_ok(), "setup_sandbox_keypair failed: {:?}", kp.err());

    let kp = kp.unwrap();
    assert!(!kp.pubkey.is_empty());
    assert!(!kp.path.is_empty());

    // Verify the keypair is funded
    let rpc2 = surfnet.rpc_url().to_string();
    let dpk = kp.pubkey.clone();
    let balance = client::balance::get_balances(&rpc2, &dpk).await.unwrap();
    assert!(
        balance.sol_lamports >= 100_000_000_000,
        "Should have 100 SOL, got {}",
        balance.sol_lamports
    );
}

// =============================================================================
// Payment middleware with real Solana (full 402 → pay → 200 flow)
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_payment_flow_with_surfnet() {
    use axum::Router;
    use axum::middleware;
    use axum::routing::any;
    use pay_core::PaymentState;
    use pay_types::metering::ApiSpec;
    use solana_mpp::server::Mpp;
    use solana_mpp::solana_keychain::memory::MemorySigner;
    use std::sync::Arc;

    #[derive(Clone)]
    struct S {
        apis: Arc<Vec<ApiSpec>>,
        mpp: Option<Mpp>,
    }
    impl PaymentState for S {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&Mpp> {
            self.mpp.as_ref()
        }
    }

    let surfnet = start_surfnet().await;
    let recipient = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&recipient.pubkey(), 1_000_000_000)
        .unwrap();

    let api: ApiSpec =
        serde_yml::from_str(&std::fs::read_to_string("tests/fixtures/test-provider.yml").unwrap())
            .unwrap();

    let mpp = Mpp::new(solana_mpp::server::Config {
        recipient: recipient.pubkey().to_string(),
        currency: "SOL".to_string(),
        decimals: 9,
        // Surfpool is a localnet implementation. Its prefixed blockhash
        // is acceptable for `network: localnet` per the SDK's
        // asymmetric check (the only place SURFNET-prefixed hashes
        // are valid).
        network: "localnet".to_string(),
        rpc_url: Some(surfnet.rpc_url().to_string()),
        secret_key: Some("test-secret".to_string()),
        ..Default::default()
    })
    .unwrap();

    let state = S {
        apis: Arc::new(vec![api]),
        mpp: Some(mpp.clone()),
    };

    let app = Router::new()
        .fallback(any(|| async {
            axum::Json(serde_json::json!({"ok": true}))
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<S>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();

    // Step 1: Get 402
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
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
        .unwrap()
        .to_string();
    let challenge = solana_mpp::parse_www_authenticate(&www_auth).unwrap();

    // Step 2: Build payment
    let payer = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&payer.pubkey(), 2_000_000_000)
        .unwrap();
    let signer = MemorySigner::from_bytes(&payer.to_bytes()).unwrap();
    let rpc =
        solana_mpp::solana_rpc_client::rpc_client::RpcClient::new(surfnet.rpc_url().to_string());
    let auth = solana_mpp::client::build_credential_header(&signer, &rpc, &challenge)
        .await
        .unwrap();

    // Step 3: Pay and get 200
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .header("authorization", &auth)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("payment-receipt").is_some());
}

// =============================================================================
// Session intent — push mode full lifecycle (challenge → open → voucher → close)
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_session_full_flow() {
    use axum::Router;
    use axum::middleware;
    use axum::routing::any;
    use pay_core::PaymentState;
    use pay_core::server::session::SessionMpp;
    use pay_types::metering::ApiSpec;
    use solana_mpp::client::session::ActiveSession;
    use solana_mpp::server::session::SessionConfig;
    use solana_mpp::solana_keychain::memory::MemorySigner;
    use solana_mpp::{
        PaymentCredential, SessionMode, format_authorization, parse_www_authenticate,
    };
    use std::sync::Arc;

    // ── App state ──────────────────────────────────────────────────────────
    #[derive(Clone)]
    struct S {
        apis: Arc<Vec<ApiSpec>>,
        session_mpp: Arc<SessionMpp>,
    }
    impl PaymentState for S {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&solana_mpp::server::Mpp> {
            None
        }
        fn session_mpp(&self) -> Option<&SessionMpp> {
            Some(&self.session_mpp)
        }
    }

    // ── Infrastructure ─────────────────────────────────────────────────────
    let surfnet = start_surfnet().await;
    let rpc_url = surfnet.rpc_url().to_string();

    let operator = Keypair::new();
    let recipient = Keypair::new();

    // Fund the client that will "deposit" into the session channel.
    let client_kp = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&client_kp.pubkey(), 2_000_000_000)
        .unwrap();

    let api: ApiSpec =
        serde_yml::from_str(&std::fs::read_to_string("tests/fixtures/test-provider.yml").unwrap())
            .unwrap();

    // 1 USDC cap (6 decimals). rpc_url enables on-chain signature verification.
    let session_mpp = SessionMpp::new(
        SessionConfig {
            operator: operator.pubkey().to_string(),
            recipient: recipient.pubkey().to_string(),
            max_cap: 1_000_000,
            currency: "USDC".to_string(),
            decimals: 6,
            network: "localnet".to_string(),
            modes: vec![SessionMode::Push],
            rpc_url: Some(rpc_url.clone()),
            ..Default::default()
        },
        "test-session-secret",
    );

    let state = S {
        apis: Arc::new(vec![api]),
        session_mpp: Arc::new(session_mpp),
    };

    let app = Router::new()
        .fallback(any(|| async {
            axum::Json(serde_json::json!({"ok": true}))
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<S>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let http = reqwest::Client::new();

    // ── Step 1: 402 session challenge ──────────────────────────────────────
    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402, "expected 402, got {}", resp.status());

    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .expect("missing www-authenticate header")
        .to_str()
        .unwrap()
        .to_string();

    let challenge = parse_www_authenticate(&www_auth).unwrap();
    assert_eq!(
        challenge.intent.as_str(),
        "session",
        "expected session intent"
    );
    assert_eq!(challenge.method.as_str(), "solana");

    // ── Step 2: Open session ───────────────────────────────────────────────
    // Session key: any Ed25519 keypair — signs vouchers, never touches chain.
    let session_kp = Keypair::new();
    let session_signer: Box<dyn solana_mpp::solana_keychain::SolanaSigner> =
        Box::new(MemorySigner::from_bytes(&session_kp.to_bytes()).unwrap());

    // Submit a real SOL transfer to surfpool as a stand-in for the Fiber
    // channel open. The server verifies this tx is confirmed on-chain before
    // accepting the open.
    let kp_file = keypair_to_file(&client_kp);
    let send_result = client::send::send_sol(
        "0.01",
        &operator.pubkey().to_string(),
        kp_file.path().to_str().unwrap(),
        &rpc_url,
    )
    .await
    .expect("deposit SOL transfer failed");
    let open_tx_sig = send_result.signature;

    // Channel ID is any valid Solana pubkey (would be the real Fiber channel
    // in production; here it's just a key for the in-memory store).
    let channel_id = Keypair::new().pubkey();
    let mut active = ActiveSession::new(channel_id, session_signer);

    let deposit = 1_000_000u64; // 1 USDC
    let open_action = active.open_action(deposit, &open_tx_sig);
    let auth =
        format_authorization(&PaymentCredential::new(challenge.to_echo(), open_action)).unwrap();

    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .header("authorization", &auth)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "open should return 200, got {}: {}",
        resp.status(),
        resp.text().await.unwrap()
    );

    // ── Step 3: Voucher (subsequent API call) ──────────────────────────────
    let voucher_action = active.voucher_action(1_000).await.unwrap(); // 0.001 USDC
    let auth =
        format_authorization(&PaymentCredential::new(challenge.to_echo(), voucher_action)).unwrap();

    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .header("authorization", &auth)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "voucher should return 200, got {}",
        resp.status()
    );

    // Second voucher — watermark advances
    let voucher_action = active.voucher_action(1_000).await.unwrap();
    let auth =
        format_authorization(&PaymentCredential::new(challenge.to_echo(), voucher_action)).unwrap();

    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .header("authorization", &auth)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "second voucher should return 200");

    // ── Step 4: Close session ──────────────────────────────────────────────
    let close_action = active.close_action(None).await.unwrap();
    let auth =
        format_authorization(&PaymentCredential::new(challenge.to_echo(), close_action)).unwrap();

    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .header("authorization", &auth)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "close should return 200, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["status"], "closed",
        "expected closed status, got {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_session_submits_required_setup_and_batches_channel_opens() {
    use axum::Router;
    use axum::middleware;
    use axum::routing::any;
    use pay_core::PaymentState;
    use pay_core::server::session::{MultiDelegateChain, SessionMpp};
    use solana_mpp::PaymentCredential;
    use solana_mpp::program::multi_delegator::MultiDelegateOnChainState;
    use solana_mpp::server::session::SessionConfig;
    use solana_mpp::solana_keychain::memory::MemorySigner;
    use solana_mpp::{SessionAction, SessionMode, format_authorization, parse_www_authenticate};
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct S {
        apis: Arc<Vec<pay_types::metering::ApiSpec>>,
        session_mpp: Arc<SessionMpp>,
    }
    impl PaymentState for S {
        fn apis(&self) -> &[pay_types::metering::ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&solana_mpp::server::Mpp> {
            None
        }
        fn session_mpp(&self) -> Option<&SessionMpp> {
            Some(&self.session_mpp)
        }
    }

    #[derive(Clone)]
    struct MockChain {
        states: Arc<HashMap<String, MultiDelegateOnChainState>>,
        submitted: Arc<Mutex<Vec<String>>>,
    }

    impl MockChain {
        fn new(states: HashMap<String, MultiDelegateOnChainState>) -> Self {
            Self {
                states: Arc::new(states),
                submitted: Arc::new(Mutex::new(vec![])),
            }
        }

        fn submitted_txs(&self) -> Vec<String> {
            self.submitted.lock().unwrap().clone()
        }
    }

    impl MultiDelegateChain for MockChain {
        fn fetch_state<'a>(
            &'a self,
            owner: &'a str,
        ) -> Pin<Box<dyn Future<Output = pay_core::Result<MultiDelegateOnChainState>> + Send + 'a>>
        {
            let state = self
                .states
                .get(owner)
                .cloned()
                .unwrap_or(MultiDelegateOnChainState {
                    multi_delegate_exists: false,
                    existing_delegation_cap: None,
                });
            Box::pin(async move { Ok(state) })
        }

        fn submit_tx<'a>(
            &'a self,
            tx_base64: &'a str,
        ) -> Pin<Box<dyn Future<Output = pay_core::Result<String>> + Send + 'a>> {
            let submitted = Arc::clone(&self.submitted);
            let tx = tx_base64.to_string();
            Box::pin(async move {
                submitted.lock().unwrap().push(tx);
                Ok("mock_sig".to_string())
            })
        }
    }

    let api: pay_types::metering::ApiSpec =
        serde_yml::from_str(&std::fs::read_to_string("tests/fixtures/test-provider.yml").unwrap())
            .unwrap();

    let owner_init = Keypair::new();
    let owner_update = Keypair::new();
    let batch_submissions: BatchLog = Arc::new(Mutex::new(vec![]));

    let chain = MockChain::new(HashMap::from([
        (
            owner_init.pubkey().to_string(),
            MultiDelegateOnChainState {
                multi_delegate_exists: false,
                existing_delegation_cap: None,
            },
        ),
        (
            owner_update.pubkey().to_string(),
            MultiDelegateOnChainState {
                multi_delegate_exists: true,
                existing_delegation_cap: Some(250_000),
            },
        ),
    ]));

    let session_mpp = SessionMpp::new(
        SessionConfig {
            operator: Keypair::new().pubkey().to_string(),
            recipient: Keypair::new().pubkey().to_string(),
            max_cap: 1_000_000,
            currency: "USDC".to_string(),
            decimals: 6,
            network: "localnet".to_string(),
            modes: vec![SessionMode::Pull],
            rpc_url: None,
            ..Default::default()
        },
        "test-session-secret",
    )
    .with_multi_delegate_chain(Box::new(chain.clone()))
    .with_test_open_channel_batcher({
        let batch_submissions = Arc::clone(&batch_submissions);
        move |batch| {
            let batch_submissions = Arc::clone(&batch_submissions);
            async move {
                batch_submissions.lock().unwrap().push(batch);
                Ok(())
            }
        }
    });

    let state = S {
        apis: Arc::new(vec![api]),
        session_mpp: Arc::new(session_mpp),
    };

    let app = Router::new()
        .fallback(any(|| async {
            axum::Json(serde_json::json!({"ok": true}))
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<S>,
        ))
        .with_state(state);

    async fn send_session_request(
        app: &Router,
        authorization: Option<String>,
    ) -> axum::response::Response {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/simple/echo")
            .header("host", "testapi.localhost")
            .body(Body::from("{}"))
            .unwrap();
        if let Some(authorization) = authorization {
            req.headers_mut()
                .insert("authorization", authorization.parse().unwrap());
        }
        app.clone().oneshot(req).await.unwrap()
    }

    let challenge = {
        let resp = send_session_request(&app, None).await;
        assert_eq!(resp.status(), 402);
        let www_auth = resp
            .headers()
            .get("www-authenticate")
            .expect("missing www-authenticate header")
            .to_str()
            .unwrap()
            .to_string();
        parse_www_authenticate(&www_auth).unwrap()
    };

    async fn send_pull_open(
        app: &Router,
        challenge: &solana_mpp::PaymentChallenge,
        owner: &Keypair,
        token_account: &Keypair,
        approve_tx_sig: &str,
        init_tx: Option<&str>,
        update_tx: Option<&str>,
    ) {
        let session_kp = Keypair::new();
        let session_signer: Box<dyn solana_mpp::solana_keychain::SolanaSigner> =
            Box::new(MemorySigner::from_bytes(&session_kp.to_bytes()).unwrap());
        let active =
            solana_mpp::client::session::ActiveSession::new(token_account.pubkey(), session_signer);
        let payload =
            match active.open_pull_action(1_000_000, &owner.pubkey().to_string(), approve_tx_sig) {
                SessionAction::Open(payload) => payload,
                _ => unreachable!("open_pull_action must return SessionAction::Open"),
            };
        let payload = match init_tx {
            Some(tx) => payload.with_init_tx(tx.to_string()),
            None => payload,
        };
        let payload = match update_tx {
            Some(tx) => payload.with_update_tx(tx.to_string()),
            None => payload,
        };
        let auth = format_authorization(&PaymentCredential::new(
            challenge.to_echo(),
            SessionAction::Open(payload),
        ))
        .unwrap();

        let resp = send_session_request(app, Some(auth)).await;
        assert_eq!(resp.status(), 200, "pull open should return 200");
    }

    let token_account_init = Keypair::new();
    let token_account_update = Keypair::new();
    send_pull_open(
        &app,
        &challenge,
        &owner_init,
        &token_account_init,
        "approve_sig_init",
        Some("init_tx_base64"),
        Some("update_tx_unused"),
    )
    .await;
    send_pull_open(
        &app,
        &challenge,
        &owner_update,
        &token_account_update,
        "approve_sig_update",
        None,
        Some("update_tx_base64"),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(550)).await;

    assert_eq!(
        chain.submitted_txs(),
        vec!["init_tx_base64".to_string(), "update_tx_base64".to_string()]
    );

    let batches = batch_submissions.lock().unwrap().clone();
    assert_eq!(batches.len(), 1, "expected one batch flush");
    assert_eq!(batches[0].len(), 2, "expected both opens in the same batch");
    assert_eq!(
        batches[0],
        vec![
            (
                owner_init.pubkey().to_string(),
                token_account_init.pubkey().to_string(),
                1_000_000,
            ),
            (
                owner_update.pubkey().to_string(),
                token_account_update.pubkey().to_string(),
                1_000_000,
            ),
        ]
    );
}

// =============================================================================
// Pull-mode full flow — real surfpool + USDC + cryptographically signed txs
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_session_full_flow() {
    use axum::Router;
    use axum::middleware;
    use axum::routing::any;
    use pay_core::PaymentState;
    use pay_core::server::session::{MultiDelegateChain, SessionMpp};
    use pay_types::metering::ApiSpec;
    use solana_mpp::client::multi_delegate::{
        build_init_multi_delegate_tx, build_update_delegation_tx,
    };
    use solana_mpp::client::session::ActiveSession;
    use solana_mpp::program::multi_delegator::{
        MULTI_DELEGATOR_PROGRAM_ID, MultiDelegateOnChainState,
    };
    use solana_mpp::server::session::SessionConfig;
    use solana_mpp::solana_keychain::memory::MemorySigner;
    use solana_mpp::{
        PaymentCredential, SessionAction, SessionMode, format_authorization, parse_www_authenticate,
    };
    use solana_pubkey::Pubkey;
    use std::future::Future;
    use std::pin::Pin;
    use std::str::FromStr;
    use std::sync::Arc;

    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

    // ── Capturing mock chain ───────────────────────────────────────────────
    // `fetch_state` returns "no delegation" (fresh user).
    // `submit_tx` records the base64 tx for post-run assertions.
    #[derive(Clone)]
    struct CapturingChain {
        submitted: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl CapturingChain {
        fn new() -> Self {
            Self {
                submitted: Arc::new(std::sync::Mutex::new(vec![])),
            }
        }
        fn take_submitted(&self) -> Vec<String> {
            self.submitted.lock().unwrap().clone()
        }
    }

    impl MultiDelegateChain for CapturingChain {
        fn fetch_state<'a>(
            &'a self,
            _owner: &'a str,
        ) -> Pin<Box<dyn Future<Output = pay_core::Result<MultiDelegateOnChainState>> + Send + 'a>>
        {
            Box::pin(async move {
                Ok(MultiDelegateOnChainState {
                    multi_delegate_exists: false,
                    existing_delegation_cap: None,
                })
            })
        }

        fn submit_tx<'a>(
            &'a self,
            tx_base64: &'a str,
        ) -> Pin<Box<dyn Future<Output = pay_core::Result<String>> + Send + 'a>> {
            let submitted = Arc::clone(&self.submitted);
            let tx = tx_base64.to_string();
            Box::pin(async move {
                println!("    [chain] submit_tx ({}...)", &tx[..40.min(tx.len())]);
                submitted.lock().unwrap().push(tx);
                Ok("captured_sig_not_submitted_to_chain".to_string())
            })
        }
    }

    #[derive(Clone)]
    struct S {
        apis: Arc<Vec<ApiSpec>>,
        session_mpp: Arc<SessionMpp>,
    }
    impl PaymentState for S {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&solana_mpp::server::Mpp> {
            None
        }
        fn session_mpp(&self) -> Option<&SessionMpp> {
            Some(&self.session_mpp)
        }
    }

    // ── [1] Start surfnet with JIT fetching from mainnet ───────────────────
    println!("\n╔══════════════════════════════════════════════════════╗");
    println!("║   PULL SESSION FULL FLOW — surfpool + signed txs     ║");
    println!("╚══════════════════════════════════════════════════════╝");

    println!("\n[1] Starting surfnet with JIT mainnet fetching...");
    let surfnet = Surfnet::builder()
        .remote_rpc_url("https://api.mainnet-beta.solana.com")
        .airdrop_sol(10_000_000_000)
        .start()
        .await
        .expect("Failed to start JIT surfnet");
    let rpc_url = surfnet.rpc_url().to_string();
    println!("    RPC: {rpc_url}");

    // ── [2] Fund user with SOL + USDC ──────────────────────────────────────
    println!("\n[2] Funding user with SOL + USDC (via surfpool cheatcode)...");
    let user_kp = Keypair::new();
    let operator_kp = Keypair::new();
    let recipient_kp = Keypair::new();

    let mint_pk = Pubkey::from_str(USDC_MINT).unwrap();
    let token_program_pk = Pubkey::from_str(SPL_TOKEN_PROGRAM).unwrap();
    let program_id_pk = Pubkey::from_str(MULTI_DELEGATOR_PROGRAM_ID).unwrap();

    surfnet
        .cheatcodes()
        .fund_sol(&user_kp.pubkey(), 2_000_000_000)
        .unwrap();
    surfnet
        .cheatcodes()
        .fund_sol(&operator_kp.pubkey(), 2_000_000_000)
        .unwrap();

    // 5 USDC (6 decimals) — surfpool JIT fetches the USDC mint from mainnet
    let usdc_amount = 5_000_000u64;
    surfnet
        .cheatcodes()
        .fund_token(&user_kp.pubkey(), &mint_pk, usdc_amount, None)
        .unwrap();
    let user_ata = surfnet
        .cheatcodes()
        .get_ata(&user_kp.pubkey(), &mint_pk, None);
    println!("    user:        {}", user_kp.pubkey());
    println!("    user_ata:    {user_ata}");
    println!("    operator:    {}", operator_kp.pubkey());
    println!("    USDC funded: {usdc_amount} (mint: {USDC_MINT})");

    // ── [3] Build SessionMpp with capturing chain ──────────────────────────
    println!("\n[3] Building SessionMpp with CapturingChain...");

    let channel_opens: BatchLog = Arc::new(Mutex::new(vec![]));

    let chain = CapturingChain::new();

    let session_mpp = SessionMpp::new(
        SessionConfig {
            operator: operator_kp.pubkey().to_string(),
            recipient: recipient_kp.pubkey().to_string(),
            max_cap: 1_000_000,
            currency: "USDC".to_string(),
            decimals: 6,
            network: "localnet".to_string(),
            modes: vec![SessionMode::Pull],
            rpc_url: None,
            ..Default::default()
        },
        "test-pull-secret",
    )
    .with_multi_delegate_chain(Box::new(chain.clone()))
    .with_test_open_channel_batcher({
        let channel_opens = Arc::clone(&channel_opens);
        move |batch| {
            let channel_opens = Arc::clone(&channel_opens);
            async move {
                println!("    [batcher] flush: {} opens", batch.len());
                for (owner, ata, cap) in &batch {
                    println!("      → owner={owner} ata={ata} cap={cap}");
                }
                channel_opens.lock().unwrap().push(batch);
                Ok(())
            }
        }
    });

    let api: ApiSpec =
        serde_yml::from_str(&std::fs::read_to_string("tests/fixtures/test-provider.yml").unwrap())
            .unwrap();

    let state = S {
        apis: Arc::new(vec![api]),
        session_mpp: Arc::new(session_mpp),
    };

    // ── [4] Start HTTP server ──────────────────────────────────────────────
    println!("\n[4] Starting HTTP server...");
    let app = Router::new()
        .fallback(any(|| async {
            axum::Json(serde_json::json!({"ok": true}))
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<S>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    println!("    URL: {url}");

    let http = reqwest::Client::new();

    // ── [5] GET 402 challenge ──────────────────────────────────────────────
    println!("\n[5] Sending initial request → 402 challenge...");
    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402, "expected 402, got {}", resp.status());

    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .expect("missing www-authenticate header")
        .to_str()
        .unwrap()
        .to_string();
    let challenge = parse_www_authenticate(&www_auth).unwrap();
    println!(
        "    challenge intent={} method={}",
        challenge.intent, challenge.method
    );

    // ── [6] Build initMultiDelegateTx + updateDelegationTx ────────────────
    println!("\n[6] Building multi-delegator transactions...");

    let user_signer: Box<dyn solana_mpp::solana_keychain::SolanaSigner> =
        Box::new(MemorySigner::from_bytes(&user_kp.to_bytes()).unwrap());

    // Get a recent blockhash from surfpool
    let recent_blockhash = {
        use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
        tokio::task::spawn_blocking({
            let rpc_url = rpc_url.clone();
            move || RpcClient::new(rpc_url).get_latest_blockhash().unwrap()
        })
        .await
        .unwrap()
    };
    println!("    blockhash: {recent_blockhash}");

    let cap = 1_000_000u64; // 1 USDC
    let expiry_ts = 9_999_999_999i64; // far future

    let init_tx_b64 = build_init_multi_delegate_tx(
        user_signer.as_ref(),
        &mint_pk,
        &user_ata,
        &operator_kp.pubkey(),
        &program_id_pk,
        &token_program_pk,
        0, // nonce
        cap,
        expiry_ts,
        recent_blockhash,
    )
    .await
    .expect("build_init_multi_delegate_tx failed");

    let update_tx_b64 = build_update_delegation_tx(
        user_signer.as_ref(),
        &mint_pk,
        &operator_kp.pubkey(),
        &program_id_pk,
        0,       // nonce
        cap * 2, // higher cap for update path
        expiry_ts,
        recent_blockhash,
    )
    .await
    .expect("build_update_delegation_tx failed");

    println!("    initMultiDelegateTx: {}...", &init_tx_b64[..40]);
    println!("    updateDelegationTx:  {}...", &update_tx_b64[..40]);

    // ── [7] Open session (pull mode) — server submits initMultiDelegateTx ─
    println!("\n[7] Opening pull-mode session (server will submit initMultiDelegateTx)...");

    let session_kp = Keypair::new();
    let session_signer: Box<dyn solana_mpp::solana_keychain::SolanaSigner> =
        Box::new(MemorySigner::from_bytes(&session_kp.to_bytes()).unwrap());

    let mut active = ActiveSession::new(user_ata, session_signer);

    let SessionAction::Open(open_payload) =
        active.open_pull_action(cap, &user_kp.pubkey().to_string(), "pull_delegation_setup")
    else {
        unreachable!("open_pull_action must return Open")
    };

    let open_payload = open_payload
        .with_init_tx(init_tx_b64.clone())
        .with_update_tx(update_tx_b64.clone());

    let auth = format_authorization(&PaymentCredential::new(
        challenge.to_echo(),
        SessionAction::Open(open_payload),
    ))
    .unwrap();

    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .header("authorization", &auth)
        .body("{}")
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body_text = resp.text().await.unwrap();
    assert_eq!(
        status, 200,
        "open should return 200, got {status}: {body_text}"
    );
    println!("    ✓ open accepted (200)");

    // Give the batcher a moment to flush
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    // ── [8] Vouchers ───────────────────────────────────────────────────────
    println!("\n[8] Sending vouchers...");
    for i in 1..=2 {
        let voucher_action = active.voucher_action(1_000).await.unwrap();
        let auth =
            format_authorization(&PaymentCredential::new(challenge.to_echo(), voucher_action))
                .unwrap();
        let resp = http
            .post(format!("{url}/v1/simple/echo"))
            .header("host", "testapi.localhost")
            .header("authorization", &auth)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "voucher {i} should return 200");
        println!("    ✓ voucher {i} accepted (200)");
    }

    // ── [9] Close ──────────────────────────────────────────────────────────
    println!("\n[9] Closing session...");
    let close_action = active.close_action(None).await.unwrap();
    let auth =
        format_authorization(&PaymentCredential::new(challenge.to_echo(), close_action)).unwrap();
    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .header("authorization", &auth)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "close should return 200");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "closed");
    println!("    ✓ session closed");

    // ── Verify server submitted the correct tx ────────────────────────────
    let submitted = chain.take_submitted();
    assert_eq!(
        submitted.len(),
        1,
        "expected one tx submitted, got {}",
        submitted.len()
    );
    assert_eq!(
        submitted[0], init_tx_b64,
        "server should have submitted initMultiDelegateTx (not updateDelegationTx)"
    );
    println!("    ✓ server submitted initMultiDelegateTx (not updateDelegationTx)");

    // ── Verify batcher received the channel open ───────────────────────────
    let opens = channel_opens.lock().unwrap().clone();
    assert_eq!(
        opens.len(),
        1,
        "expected one batch flush, got {}",
        opens.len()
    );
    assert_eq!(opens[0].len(), 1, "expected one channel open in the batch");
    assert_eq!(opens[0][0].0, user_kp.pubkey().to_string());
    assert_eq!(opens[0][0].1, user_ata.to_string());
    assert_eq!(opens[0][0].2, cap);
    println!(
        "    ✓ batcher received channel open for owner={}",
        opens[0][0].0
    );

    println!("\n╔══════════════════════════════════════════════════════╗");
    println!("║   ALL STEPS PASSED ✓                                 ║");
    println!("╚══════════════════════════════════════════════════════╝\n");
}

// =============================================================================
// MPP build_credential (pay_core::client::mpp)
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpp_build_credential_with_surfnet() {
    use axum::Router;
    use axum::middleware;
    use axum::routing::any;
    use pay_core::PaymentState;

    use pay_types::metering::ApiSpec;
    use solana_mpp::server::Mpp;
    use std::sync::Arc;

    #[derive(Clone)]
    struct S {
        apis: Arc<Vec<ApiSpec>>,
        mpp: Option<Mpp>,
    }
    impl PaymentState for S {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&Mpp> {
            self.mpp.as_ref()
        }
    }

    let surfnet = start_surfnet().await;
    let recipient = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&recipient.pubkey(), 1_000_000_000)
        .unwrap();

    let api: ApiSpec =
        serde_yml::from_str(&std::fs::read_to_string("tests/fixtures/test-provider.yml").unwrap())
            .unwrap();

    let mpp = Mpp::new(solana_mpp::server::Config {
        recipient: recipient.pubkey().to_string(),
        currency: "SOL".to_string(),
        decimals: 9,
        // Surfpool is a localnet implementation. Its prefixed blockhash
        // is acceptable for `network: localnet` per the SDK's
        // asymmetric check (the only place SURFNET-prefixed hashes
        // are valid).
        network: "localnet".to_string(),
        rpc_url: Some(surfnet.rpc_url().to_string()),
        secret_key: Some("test-secret".to_string()),
        ..Default::default()
    })
    .unwrap();

    let state = S {
        apis: Arc::new(vec![api]),
        mpp: Some(mpp),
    };

    let app = Router::new()
        .fallback(any(|| async {
            axum::Json(serde_json::json!({"ok": true}))
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<S>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Step 1: Get a 402 challenge
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
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
        .unwrap()
        .to_string();
    let challenge = client::mpp::parse(&www_auth).unwrap();

    // Step 2: Create a funded payer (the new network-aware path takes
    // raw secret bytes via a MemoryAccountsStore, no temp file needed).
    let payer = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&payer.pubkey(), 2_000_000_000)
        .unwrap();

    // Step 3: Build credential using pay_core's network-aware path.
    //
    // Inject the test payer into a MemoryAccountsStore as an ephemeral
    // account mapped to `localnet` — that's how the new
    // `build_credential(challenge, store, network_override, account_override)` API
    // resolves the wallet (no more `keypair_source: &str`).
    //
    // build_credential creates its own tokio runtime, so we drive it
    // from a blocking thread.
    let rpc_url = surfnet.rpc_url().to_string();
    let challenge_clone = challenge.clone();
    let payer_bytes = payer.to_bytes().to_vec();
    let payer_pubkey = payer.pubkey().to_string();
    let auth = tokio::task::spawn_blocking(move || {
        // SAFETY: test-only env manipulation, runs before any other
        // threads in this closure.
        unsafe { std::env::set_var("PAY_RPC_URL", &rpc_url) };

        let mut file = pay_core::accounts::AccountsFile::default();
        file.upsert(
            "localnet",
            "default",
            pay_core::accounts::Account {
                keystore: pay_core::accounts::Keystore::Ephemeral,
                active: false,
                auth_required: Some(false),
                pubkey: Some(payer_pubkey),
                vault: None,
                path: None,
                secret_key_b58: Some(bs58::encode(&payer_bytes).into_string()),
                created_at: Some("2026-04-10T00:00:00Z".to_string()),
            },
        );
        let store = pay_core::accounts::MemoryAccountsStore::with_file(file);

        let result =
            client::mpp::build_credential(&challenge_clone, &store, Some("localnet"), None);
        unsafe { std::env::remove_var("PAY_RPC_URL") };
        result
    })
    .await
    .unwrap();

    assert!(auth.is_ok(), "build_credential failed: {:?}", auth.err());
    let (auth, ephemeral) = auth.unwrap();
    assert!(!auth.is_empty());
    assert!(
        ephemeral.is_none(),
        "should be a cache hit (we pre-populated the store)"
    );

    // Step 4: Use the credential — should get 200
    let resp = http
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .header("authorization", &auth)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
