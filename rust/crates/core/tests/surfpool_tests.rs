//! Integration tests using surfpool-sdk (embedded Solana validator).
//!
//! Tests the client modules (balance, send, dev) and server modules
//! (payment middleware) against a real Solana runtime — no external
//! process needed.
//!
//! Run: `cargo test -p pay-core --features server --test surfpool_tests`

#![cfg(feature = "server")]

use pay_core::client;
use surfpool_sdk::{Keypair, Signer, Surfnet};

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
    // `build_credential(challenge, store, network_override)` API
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
                pubkey: Some(payer_pubkey),
                vault: None,
                path: None,
                secret_key_b58: Some(bs58::encode(&payer_bytes).into_string()),
                created_at: Some("2026-04-10T00:00:00Z".to_string()),
            },
        );
        let store = pay_core::accounts::MemoryAccountsStore::with_file(file);

        let result = client::mpp::build_credential(&challenge_clone, &store, Some("localnet"));
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
