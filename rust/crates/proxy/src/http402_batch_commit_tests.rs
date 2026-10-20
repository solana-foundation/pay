//! Post-response x402 `batch-settlement` commitment must gate the release of a
//! successful upstream response on both Pingora response paths.

use super::*;
use ed25519_dalek::{Signer, SigningKey};
use pay_core::server::gate::{GateDecision, GateRequest, PaymentGate};
use pay_kit::core::store::{ChannelState, ChannelStore, MemoryChannelStore};
use pay_kit::mpp::program::payment_channels as pc;
use pay_kit::solana_keychain::{MemorySigner, TransactionSigner};
use pay_kit::x402::batch_settlement::{
    BatchChannelConfig, BatchPayload, BatchPaymentPayload, BatchRequirements, BatchVoucher,
    VOUCHER_EXPIRES_AT,
};
use pay_kit::x402::server::{BatchConfig, X402BatchSettlement};
use pay_types::metering::{
    ApiCategory, BatchSettlementSpec, BillingUnit, MeterDimension, MeterDirection, Metering,
    PriceTier, RoutingConfig, Scheme,
};
use pingora::proxy::ProxyHttp;
use solana_pubkey::Pubkey;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

const PAY_TO: &str = "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY";
const PAYER_SEED: u8 = 22;

#[derive(Clone)]
struct State {
    apis: Vec<ApiSpec>,
    batch: X402BatchSettlement,
}

impl PaymentState for State {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&pay_kit::mpp::server::Mpp> {
        None
    }
    fn x402_batch(&self) -> Option<&X402BatchSettlement> {
        Some(&self.batch)
    }
}

fn signer(seed: u8) -> Arc<dyn TransactionSigner> {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let mut keypair = [0u8; 64];
    keypair[..32].copy_from_slice(signing.as_bytes());
    keypair[32..].copy_from_slice(signing.verifying_key().as_bytes());
    Arc::new(MemorySigner::from_bytes(&keypair).expect("test signer"))
}

fn payer() -> (SigningKey, Pubkey) {
    let signing = SigningKey::from_bytes(&[PAYER_SEED; 32]);
    let pubkey = Pubkey::from(signing.verifying_key().to_bytes());
    (signing, pubkey)
}

fn api() -> ApiSpec {
    ApiSpec {
        name: "batch".into(),
        subdomain: "paid".into(),
        title: "batch".into(),
        description: String::new(),
        category: ApiCategory::AiMl,
        version: "1".into(),
        env: Default::default(),
        routing: RoutingConfig::Respond {},
        accounting: Default::default(),
        endpoints: vec![pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Get,
            path: "resource".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.001,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                    meter: None,
                }],
                schemes: Some(vec![Scheme::X402BatchSettlement]),
                ..Default::default()
            }),
            subscription: None,
        }],
        free_tier: None,
        quotas: None,
        notes: None,
        operator: None,
        recipients: Default::default(),
        session: None,
        batch_settlement: Some(BatchSettlementSpec::default()),
    }
}

fn payment_header(requirements: &BatchRequirements, fee_payer: &Pubkey) -> (String, String) {
    let (payer_signing, payer) = payer();
    let config = BatchChannelConfig {
        payer: payer.to_string(),
        payer_authorizer: payer.to_string(),
        receiver: requirements.pay_to.clone(),
        receiver_authorizer: None,
        token: requirements.asset.clone(),
        withdraw_delay: requirements.extra.withdraw_delay,
        salt: "42".into(),
        open_slot: 341_000_000,
        voucher_signer: None,
    };
    let channel = pay_kit::x402::protocol::schemes::batch_settlement::derive_channel_id(
        &config,
        &fee_payer.to_string(),
        &pc::default_program_id(),
    )
    .expect("channel PDA");
    let message = pc::voucher_message_bytes(&channel, 3_000, VOUCHER_EXPIRES_AT).unwrap();
    let voucher = BatchVoucher {
        channel_id: channel.to_string(),
        max_claimable_amount: "3000".into(),
        expires_at: VOUCHER_EXPIRES_AT,
        signature: bs58::encode(payer_signing.sign(&message).to_bytes()).into_string(),
    };
    let envelope = BatchPaymentPayload {
        x402_version: pay_kit::x402::X402_VERSION_V2,
        accepted: requirements.clone(),
        payload: BatchPayload::Voucher {
            channel_config: config,
            voucher,
        },
    };
    (
        channel.to_string(),
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            serde_json::to_vec(&envelope).unwrap(),
        ),
    )
}

fn channel_state(channel_id: &str, payer: &Pubkey, rent_payer: &Pubkey) -> ChannelState {
    ChannelState {
        channel_id: channel_id.to_string(),
        authorized_signer: payer.to_string(),
        deposit: 5_000,
        cumulative: 2_000,
        sealed: false,
        highest_voucher_signature: None,
        highest_voucher_expires_at: None,
        close_requested_at: None,
        final_cumulative: None,
        open_slot: Some(341_000_000),
        payer: payer.to_string(),
        rent_payer: rent_payer.to_string(),
        opening_challenge_id: String::new(),
        authentication: None,
        voucher_signer: "client".into(),
        idle_timeout_seconds: None,
        last_activity_at: 0,
        spent_amount: 2_000,
        settled_on_chain: 2_000,
        distributed_on_chain: 0,
        processed_uses: vec![],
        processed_topup_signatures: vec![],
        next_delivery_sequence: 0,
        pending_deliveries: vec![],
        committed_deliveries: std::collections::VecDeque::new(),
        pending_setup: None,
        onchain_checked_at: now(),
        lifecycle: None,
        schema_version: 2,
        extra: Default::default(),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A gate state plus a forwarded batch authorization for a valid voucher.
async fn forwarded_batch() -> (State, Arc<MemoryChannelStore>, String, BatchForward) {
    let fee_payer = signer(21);
    let fee_payer_pubkey = fee_payer.pubkey();
    let store = Arc::new(MemoryChannelStore::new());
    let mut config = BatchConfig::new(PAY_TO, "localnet", fee_payer);
    config.withdraw_delay = 3_600;
    let batch = X402BatchSettlement::with_store(config, store.clone()).unwrap();
    let requirements = batch.requirements("0.001").unwrap();
    let (channel_id, header) = payment_header(&requirements, &fee_payer_pubkey);
    let (_, payer) = payer();
    store
        .put_channel(
            &channel_id,
            channel_state(&channel_id, &payer, &fee_payer_pubkey),
        )
        .await
        .unwrap();
    let state = State {
        apis: vec![api()],
        batch,
    };
    let method = http::Method::GET;
    let request = GateRequest {
        method: &method,
        path: "resource",
        host: Some("paid.example"),
        accept: None,
        authorization: None,
        content_length: None,
        query: None,
        x402_payment: Some(&header),
    };
    let GateDecision::Forward {
        batch: Some(forward),
        ..
    } = PaymentGate::new(state.clone()).evaluate(&request).await
    else {
        panic!("valid voucher must reach the protected forward");
    };
    (state, store, channel_id, *forward)
}

async fn request_session() -> (pingora::proxy::Session, tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    client
        .write_all(b"GET /resource HTTP/1.1\r\nHost: paid.example\r\n\r\n")
        .await
        .unwrap();
    let stream = pingora::protocols::l4::stream::Stream::from(server);
    let mut session = pingora::proxy::Session::new_h1(Box::new(stream));
    assert!(session.read_request().await.unwrap());
    (session, client)
}

async fn read_response(client: &mut tokio::net::TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buf = vec![0u8; 4096];
    // Header and body may arrive in separate writes; read until the peer idles.
    while let Ok(Ok(n)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), client.read(&mut buf)).await
    {
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&raw).into_owned()
}

#[tokio::test]
async fn streaming_response_is_withheld_when_batch_commit_fails() {
    let (state, store, channel_id, forward) = forwarded_batch().await;
    store.delete_channel(&channel_id).await.unwrap();
    let gate = Http402Gate::new(state, "127.0.0.1:1");
    let (mut session, _client) = request_session().await;
    let mut ctx = gate.new_ctx();
    ctx.batch = Some(forward);
    let mut response = ResponseHeader::build(200, None).unwrap();

    let error = gate
        .response_filter(&mut session, &mut response, &mut ctx)
        .await
        .expect_err("an unconfirmed charge must not release the response");

    assert_eq!(*error.etype(), pingora::ErrorType::HTTPStatus(502));
    assert!(response.headers.get("payment-response").is_none());
    assert!(ctx.batch.is_none());
    assert!(
        ctx.batch_cache.is_none(),
        "unpaid responses are never cached"
    );
}

#[tokio::test]
async fn streaming_response_carries_receipt_when_batch_commit_succeeds() {
    let (state, store, channel_id, forward) = forwarded_batch().await;
    let gate = Http402Gate::new(state, "127.0.0.1:1");
    let (mut session, _client) = request_session().await;
    let mut ctx = gate.new_ctx();
    ctx.batch = Some(forward);
    let mut response = ResponseHeader::build(200, None).unwrap();

    gate.response_filter(&mut session, &mut response, &mut ctx)
        .await
        .expect("a confirmed charge releases the response");

    assert_eq!(response.status.as_u16(), 200);
    assert!(response.headers.get("payment-response").is_some());
    let channel = store.get_channel(&channel_id).await.unwrap().unwrap();
    assert_eq!(channel.cumulative, 3_000, "the voucher was committed");
}

#[tokio::test]
async fn buffered_response_is_replaced_when_batch_commit_fails() {
    let (state, store, channel_id, forward) = forwarded_batch().await;
    store.delete_channel(&channel_id).await.unwrap();
    let gate = Http402Gate::new(state, "127.0.0.1:1");
    let (mut session, mut client) = request_session().await;
    let mut ctx = gate.new_ctx();
    ctx.batch = Some(forward);
    let served = axum::response::Response::builder()
        .status(200)
        .body(axum::body::Body::from("protected-content"))
        .unwrap();

    gate.finish_inline(&mut session, &mut ctx, served)
        .await
        .unwrap();

    let raw = read_response(&mut client).await;
    assert!(raw.starts_with("HTTP/1.1 502"), "{raw}");
    assert!(!raw.contains("protected-content"));
    assert!(raw.contains("payment_commit_failed"));
}

#[tokio::test]
async fn buffered_response_is_released_when_batch_commit_succeeds() {
    let (state, _store, _channel_id, forward) = forwarded_batch().await;
    let gate = Http402Gate::new(state, "127.0.0.1:1");
    let (mut session, mut client) = request_session().await;
    let mut ctx = gate.new_ctx();
    ctx.batch = Some(forward);
    let served = axum::response::Response::builder()
        .status(200)
        .body(axum::body::Body::from("protected-content"))
        .unwrap();

    gate.finish_inline(&mut session, &mut ctx, served)
        .await
        .unwrap();

    let raw = read_response(&mut client).await;
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    assert!(raw.to_ascii_lowercase().contains("payment-response"));
    assert!(raw.contains("protected-content"));
}

#[tokio::test]
async fn streaming_redirect_commits_the_batch_voucher() {
    for (status, committed) in [(303, 3_000), (307, 3_000), (404, 2_000), (500, 2_000)] {
        let (state, store, channel_id, forward) = forwarded_batch().await;
        let gate = Http402Gate::new(state, "127.0.0.1:1");
        let (mut session, _client) = request_session().await;
        let mut ctx = gate.new_ctx();
        ctx.batch = Some(forward);
        let mut response = ResponseHeader::build(status, None).unwrap();

        gate.response_filter(&mut session, &mut response, &mut ctx)
            .await
            .unwrap();

        let channel = store.get_channel(&channel_id).await.unwrap().unwrap();
        assert_eq!(channel.cumulative, committed, "status {status}");
        assert_eq!(
            response.headers.get("payment-response").is_some(),
            committed > 2_000,
            "status {status}"
        );
    }
}

#[tokio::test]
async fn buffered_redirect_commits_the_batch_voucher() {
    for (status, committed) in [(302, 3_000), (400, 2_000)] {
        let (state, store, channel_id, forward) = forwarded_batch().await;
        let gate = Http402Gate::new(state, "127.0.0.1:1");
        let (mut session, mut client) = request_session().await;
        let mut ctx = gate.new_ctx();
        ctx.batch = Some(forward);
        let served = axum::response::Response::builder()
            .status(status)
            .header("location", "/done")
            .body(axum::body::Body::empty())
            .unwrap();

        gate.finish_inline(&mut session, &mut ctx, served)
            .await
            .unwrap();

        let raw = read_response(&mut client).await;
        assert!(raw.starts_with(&format!("HTTP/1.1 {status}")), "{raw}");
        let channel = store.get_channel(&channel_id).await.unwrap().unwrap();
        assert_eq!(channel.cumulative, committed, "status {status}");
    }
}
