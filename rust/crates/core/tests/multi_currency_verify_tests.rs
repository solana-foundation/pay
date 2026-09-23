//! A gateway that accepts several stablecoins verifies a charge credential
//! against the server for the currency the client actually paid with, and
//! reports that server's error.
//!
//! Regression: the gate used to try every configured server in order and
//! surface the last error, so a failed USDC payment came back as
//! "Currency mismatch: credential has USDC but endpoint expects USDG".
//!
//! Run: `cargo test -p pay-core --features server --test multi_currency_verify_tests`

#![cfg(feature = "server")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::any;
use ed25519_dalek::{Signer as _, SigningKey};
use pay_core::PaymentState;
use pay_kit::mpp::server::Mpp;
use pay_kit::mpp::{ChargeRequest, PaymentChallenge, format_authorization, parse_www_authenticate};
use pay_types::metering::ApiSpec;
use pay_types::stablecoin_mints::{USDC_MAINNET, USDG_MAINNET};
use serde_json::Value;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::str::FromStr;
use std::sync::Arc;

const RECIPIENT: &str = "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY";

#[derive(Clone)]
struct TestState {
    apis: Arc<Vec<ApiSpec>>,
    mpps: Arc<Vec<Mpp>>,
}

impl PaymentState for TestState {
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

async fn echo_handler(_req: Request<Body>) -> impl IntoResponse {
    axum::Json(serde_json::json!({"upstream": "ok"}))
}

fn mainnet_mpp(currency: &str) -> Mpp {
    Mpp::new(pay_kit::mpp::server::Config {
        recipient: RECIPIENT.to_string(),
        currency: currency.to_string(),
        decimals: 6,
        network: "mainnet".to_string(),
        // Unreachable: every verification must fail before any RPC call.
        rpc_url: Some("http://127.0.0.1:1/never".to_string()),
        challenge_binding_secret: Some("test-secret-key-do-not-use-32b-pad".to_string()),
        ..Default::default()
    })
    .unwrap()
}

/// Start a gateway accepting USDC and USDG, in that order.
async fn start_server() -> (String, tokio::task::JoinHandle<()>) {
    let api: ApiSpec =
        serde_yml::from_str(&std::fs::read_to_string("tests/fixtures/test-paywall.yml").unwrap())
            .unwrap();
    let state = TestState {
        apis: Arc::new(vec![api]),
        mpps: Arc::new(vec![mainnet_mpp(USDC_MAINNET), mainnet_mpp(USDG_MAINNET)]),
    };
    let app = Router::new()
        .fallback(any(echo_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<TestState>,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (url, handle)
}

/// Wrap a signed (but never valid on-chain) transaction as a credential
/// echoing `challenge`. The Surfpool-prefixed blockhash makes the server
/// reject it before any RPC call, with a message naming the network.
fn credential_with_surfpool_blockhash(challenge: &PaymentChallenge, payer: &SigningKey) -> String {
    let payer_pk = Pubkey::new_from_array(payer.verifying_key().to_bytes());
    let recipient = Pubkey::from_str(RECIPIENT).unwrap();
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&1_000u64.to_le_bytes());
    let ix = Instruction {
        program_id: Pubkey::from_str("11111111111111111111111111111111").unwrap(),
        accounts: vec![
            AccountMeta::new(payer_pk, true),
            AccountMeta::new(recipient, false),
        ],
        data,
    };
    let bytes = bs58::decode("SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxx1892bcad")
        .into_vec()
        .unwrap();
    let mut arr = [0u8; 32];
    let take = bytes.len().min(32);
    arr[32 - take..].copy_from_slice(&bytes[..take]);
    let mut tx = pay_kit::core::tx::build_unsigned(
        pay_kit::core::tx::TxVersion::V0,
        &payer_pk,
        &[ix],
        Hash::new_from_array(arr),
        None,
    )
    .unwrap();
    let sig = payer.sign(&tx.message.serialize());
    tx.signatures = vec![solana_signature::Signature::from(sig.to_bytes())];
    let payload = serde_json::json!({
        "type": "transaction",
        "transaction": pay_kit::core::tx::encode(&tx).unwrap(),
    });
    format_authorization(&pay_kit::mpp::PaymentCredential::new(
        challenge.to_echo(),
        payload,
    ))
    .unwrap()
}

fn currency_of(challenge: &PaymentChallenge) -> String {
    challenge
        .request
        .decode::<ChargeRequest>()
        .unwrap()
        .currency
}

async fn fetch_challenges(client: &reqwest::Client, url: &str) -> Vec<PaymentChallenge> {
    let resp = client
        .post(format!("{url}/v1/simple/echo"))
        .header("host", "testapi.localhost")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402, "expected initial 402");
    resp.headers()
        .get_all("www-authenticate")
        .iter()
        .map(|v| parse_www_authenticate(v.to_str().unwrap()).unwrap())
        .collect()
}

async fn pay(client: &reqwest::Client, url: &str, auth_header: &str) -> Value {
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client
            .post(format!("{url}/v1/simple/echo"))
            .header("host", "testapi.localhost")
            .header("authorization", auth_header)
            .body("{}")
            .send(),
    )
    .await
    .expect("retry must complete before any RPC timeout")
    .unwrap();
    assert_eq!(resp.status(), 402, "expected 402 verification_failed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"].as_str(),
        Some("verification_failed"),
        "{body}"
    );
    body
}

/// A USDC credential fails with the USDC server's own error, not with a
/// currency mismatch against the last configured server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usdc_credential_reports_usdc_servers_error() {
    let (url, _h) = start_server().await;
    let client = reqwest::Client::new();

    let challenges = fetch_challenges(&client, &url).await;
    let currencies: Vec<String> = challenges.iter().map(currency_of).collect();
    assert_eq!(
        currencies,
        vec![USDC_MAINNET, USDG_MAINNET],
        "one challenge per currency"
    );

    let usdc_challenge = &challenges[0];
    let auth = credential_with_surfpool_blockhash(
        usdc_challenge,
        &SigningKey::generate(&mut rand::rngs::OsRng),
    );
    let body = pay(&client, &url, &auth).await;
    let message = body["message"].as_str().unwrap_or("");

    assert!(
        !message.contains("Currency mismatch"),
        "USDG server's mismatch leaked over the USDC failure: {message}"
    );
    assert!(!message.contains(USDG_MAINNET), "{message}");
    assert!(
        message.contains("Signed against localnet"),
        "expected the USDC server's own verification error, got: {message}"
    );
}

/// A credential in a currency no server settles is rejected with the list of
/// accepted currencies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unaccepted_currency_lists_accepted_ones() {
    let (url, _h) = start_server().await;
    let client = reqwest::Client::new();

    let usdc_challenge = fetch_challenges(&client, &url).await.remove(0);
    // Forge a challenge echo in a currency the gateway does not accept. The
    // HMAC id no longer matches, but currency routing runs first.
    let mut request: ChargeRequest = usdc_challenge.request.decode().unwrap();
    request.currency = pay_types::stablecoin_mints::PYUSD_MAINNET.to_string();
    let mut forged = usdc_challenge.clone();
    forged.request = pay_kit::mpp::Base64UrlJson::from_typed(&request).unwrap();

    let auth =
        credential_with_surfpool_blockhash(&forged, &SigningKey::generate(&mut rand::rngs::OsRng));
    let body = pay(&client, &url, &auth).await;
    let message = body["message"].as_str().unwrap_or("");

    assert!(
        message.contains("is not accepted by this endpoint"),
        "{message}"
    );
    assert!(
        message.contains(USDC_MAINNET) && message.contains(USDG_MAINNET),
        "{message}"
    );
    assert_eq!(body["retryable"].as_bool(), Some(false));
}
