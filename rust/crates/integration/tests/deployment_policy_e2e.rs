//! Run only against the runner's local Surfpool (real payment-channels program)
//! and dedicated Redis. Missing infrastructure is a failure, never a skip.
#[cfg(not(unix))]
compile_error!("deployment-e2e requires a Unix host for controlled Pingora shutdown");

mod support;

use std::{str::FromStr, sync::Arc, time::Duration};

use ed25519_dalek::SigningKey;
use pay_core::client::session::SessionHandle;
use pay_kit::mpp::{
    PaymentChallenge, PaymentCredential, SessionAction, SessionAuthentication, UsePayload,
    client::{
        PaymentChannelOpenOptions, PaymentChannelSessionOpenOptions,
        create_payment_channel_session_opener, session::ActiveSession,
    },
    format_authorization,
    program::payment_channels::PAYMENT_CHANNELS_PROGRAM_ID,
    server::session::{SessionConfig, VoucherSigner},
    settlement::testkit::{fund_sol, fund_token},
    solana_keychain::{SolanaSigner, memory::MemorySigner},
};
use reqwest::StatusCode;
use solana_pubkey::Pubkey;
use support::deployment::{BODY, HOST_A, HOST_B, Harness, LocalEndpoints, now, policy};

const DEPOSIT: u64 = 2_000_000;
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

fn split_policy(host: &str, recipients: &[Pubkey; 3], version: u64) -> serde_json::Value {
    let mut policy = policy(host, 50_000, &recipients[0].to_string(), version);
    policy["allocations"] = serde_json::json!([
        {"recipient": recipients[0].to_string(), "basis_points": 6000, "amount_micro_usd": 30_000},
        {"recipient": recipients[1].to_string(), "basis_points": 3000, "amount_micro_usd": 15_000},
        {"recipient": recipients[2].to_string(), "basis_points": 1000, "amount_micro_usd": 5_000},
    ]);
    policy
}

async fn token_balance(rpc_url: &str, owner: &Pubkey, mint: &str) -> u64 {
    let token = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
    let mint = Pubkey::from_str(mint).unwrap();
    let associated =
        Pubkey::from_str(pay_kit::mpp::program::payment_channels::ASSOCIATED_TOKEN_PROGRAM)
            .unwrap();
    let (ata, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token.as_ref(), mint.as_ref()],
        &associated,
    );
    let rpc =
        pay_kit::mpp::solana_rpc_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
            rpc_url.to_string(),
            solana_commitment_config::CommitmentConfig::confirmed(),
        );
    rpc.get_token_account_balance(&ata)
        .await
        .expect("recipient token account")
        .amount
        .parse()
        .unwrap()
}

async fn run_worker(rpc_url: &str, redis_url: &str, prefix: &str, operator: &SigningKey) {
    let binary = std::env::var("PAY_DEPLOYMENT_TEST_WORKER")
        .expect("runner must build and supply the settlement worker");
    assert!(std::path::Path::new(&binary).is_absolute());
    let mut command = tokio::process::Command::new(binary);
    // The child receives only local test configuration. Do not inherit KMS,
    // production RPCs, telemetry exporters, or cleanup credentials.
    command
        .env_clear()
        .env("NETWORK", "localnet")
        .env("RPC_URL", rpc_url)
        .env("PAY_MPP_REDIS_URL", redis_url)
        .env("PAY_MPP_REDIS_PREFIX", prefix)
        .env("PAY_X402_REDIS_PREFIX", format!("{prefix}unused-batch:"))
        .env("RUN_ONCE", "true")
        .env("DRY_RUN", "false")
        .env("RUST_LOG", "info")
        .env(
            "PAY_API_SEND__FEE_PAYER__PUBKEY",
            signer(operator).pubkey().to_string(),
        )
        .env(
            "LOCAL_FEE_PAYER_PRIVATE_KEY",
            serde_json::to_string(operator.to_keypair_bytes().as_slice()).unwrap(),
        )
        .env("JOBS_CONFIRM_TIMEOUT_SECONDS", "15")
        .env("JOBS_RPC_TIMEOUT_MS", "5000")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("settlement worker deadline")
        .expect("start settlement worker");
    assert!(
        output.status.success(),
        "worker failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
            && value["fields"]["event"] == "settle_sessions_summary"
            && value["fields"]["scheme"] == "mpp/session"
        {
            eprintln!("{line}");
        }
    }
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&rand::random())
}

fn signer(key: &SigningKey) -> MemorySigner {
    MemorySigner::from_bytes(&key.to_keypair_bytes()).unwrap()
}

struct Opened {
    active: ActiveSession,
    challenge: PaymentChallenge,
    authentication: SessionAuthentication,
}

impl Opened {
    fn id(&self) -> String {
        self.active.channel_id_str()
    }

    fn use_header(&self) -> String {
        format_authorization(&PaymentCredential::new(
            self.challenge.to_echo(),
            SessionAction::Use(UsePayload {
                channel_id: self.id(),
                authentication: self.authentication.clone(),
            }),
        ))
        .unwrap()
    }
}

async fn open(harness: &Harness, host: &str, payer: &SigningKey, price: u64) -> Opened {
    let response = harness.request(host, None).await;
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let header = response.headers()["www-authenticate"].to_str().unwrap();
    let (challenge, request) = SessionHandle::parse_challenge(header).expect("session challenge");
    assert_eq!(request.amount, price.to_string());
    assert_eq!(
        request.method_details.voucher_signer,
        Some(VoucherSigner::Operator)
    );
    // Surfpool exposes blockhash context at its processed tip, one slot before
    // the confirmed bank used to verify opens. Wait without changing the
    // challenged slot or bypassing the server's open-slot verification.
    let rpc = pay_kit::mpp::solana_rpc_client::nonblocking::rpc_client::RpcClient::new(
        harness.endpoints.rpc.clone(),
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while rpc
            .get_slot_with_commitment(solana_commitment_config::CommitmentConfig::confirmed())
            .await
            .unwrap()
            < request.method_details.recent_slot.unwrap()
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("local confirmed bank reaches the challenged slot");
    let mut opened = create_payment_channel_session_opener(
        &request,
        &signer(payer),
        Box::new(signer(&key())),
        None,
        PaymentChannelSessionOpenOptions {
            open: PaymentChannelOpenOptions {
                deposit: Some(DEPOSIT),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .expect("build real funded open transaction");
    let authentication = SessionAuthentication::sign(
        challenge.id.clone(),
        &opened.session.channel_id_str(),
        payer,
    )
    .unwrap();
    let SessionAction::Open(payload) = &mut opened.action else {
        panic!("expected open")
    };
    payload.authentication = Some(authentication.clone());
    let authorization =
        format_authorization(&PaymentCredential::new(challenge.to_echo(), opened.action)).unwrap();
    let response = harness.request(host, Some(&authorization)).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "funded open response: {body}");
    assert_eq!(body, BODY);
    let opened = Opened {
        active: opened.session,
        challenge,
        authentication,
    };
    assert_eq!(harness.channel(&opened.id()).await.spent_amount, price);
    opened
}

async fn delivered(harness: &Harness, host: &str, opened: &Opened, spent: u64) {
    let response = harness.request(host, Some(&opened.use_header())).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, BODY);
    assert_eq!(harness.channel(&opened.id()).await.spent_amount, spent);
}

async fn rejected_without_charge(harness: &Harness, host: &str, opened: &Opened) {
    let before = harness.channel(&opened.id()).await.spent_amount;
    let response = harness.request(host, Some(&opened.use_header())).await;
    assert!(!response.status().is_success(), "unexpected paid success");
    assert_ne!(response.text().await.unwrap(), BODY);
    assert_eq!(harness.channel(&opened.id()).await.spent_amount, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deployment_policy_http_funding_isolation_failure_and_reconnect() {
    pay_proxy::install_crypto_provider();
    let endpoints = LocalEndpoints::from_env();
    let operator_key = key();
    let operator = Arc::new(signer(&operator_key));
    let payer_key = key();
    let payer = signer(&payer_key);
    let recipients_a = [
        signer(&key()).pubkey(),
        signer(&key()).pubkey(),
        signer(&key()).pubkey(),
    ];
    let recipient_b = signer(&key()).pubkey();
    let recipient_updated = signer(&key()).pubkey();
    let mint = pay_types::Stablecoin::Usdc.mint(Some("localnet"));
    let treasury = pay_kit::mpp::program::payment_channels::treasury_owner_for_cluster("localnet");
    for wallet in [
        operator.pubkey(),
        payer.pubkey(),
        recipients_a[0],
        recipients_a[1],
        recipients_a[2],
        recipient_b,
        recipient_updated,
        treasury,
    ] {
        fund_sol(&endpoints.rpc, &wallet, 2_000_000_000).await;
        fund_token(&endpoints.rpc, &wallet, mint, 20_000_000, TOKEN_PROGRAM).await;
    }
    let config = SessionConfig {
        operator: operator.pubkey().to_string(),
        recipient: operator.pubkey().to_string(),
        currency: mint.into(),
        decimals: 6,
        network: "localnet".into(),
        rpc_url: Some(endpoints.rpc.clone()),
        channel_program: Some(Pubkey::from_str(PAYMENT_CHANNELS_PROGRAM_ID).unwrap()),
        token_program: Some(Pubkey::from_str(TOKEN_PROGRAM).unwrap()),
        voucher_signer: VoucherSigner::Operator,
        idle_timeout_seconds: 2,
        operator_signing_key: Some(operator_key),
        suggested_deposit: Some(DEPOSIT),
        ..Default::default()
    };
    let mut harness = Harness::start(endpoints, config, operator).await;
    let original_a = split_policy(HOST_A, &recipients_a, 1);
    let original_b = policy(HOST_B, 100_000, &recipient_b.to_string(), 1);
    harness.controls.set_policy(HOST_A, original_a.clone());
    harness.controls.set_policy(HOST_B, original_b);

    // A wildcard host never falls through to the fleet x402 backend.
    let response = harness
        .client
        .get(format!("{}/invoke", harness.gateway.url))
        .header("host", HOST_A)
        .header("payment-signature", "invalid-x402-payment")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("unsupported_deployment_payment_scheme")
    );
    let response = harness
        .client
        .get(format!("{}/invoke", harness.gateway.url))
        .header("host", "backend.run.app")
        .header("x-pay-forwarded-host", HOST_A)
        .header("x-forwarded-host", HOST_A)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let a = open(&harness, HOST_A, &payer_key, 50_000).await;
    let b = open(&harness, HOST_B, &payer_key, 100_000).await;
    assert_ne!(a.id(), b.id());
    rejected_without_charge(&harness, HOST_B, &a).await;
    rejected_without_charge(&harness, HOST_A, &b).await;

    // A completed upstream error is forwarded, but never charged.
    *harness.controls.upstream_status.write().unwrap() = StatusCode::BAD_GATEWAY;
    let response = harness.request(HOST_A, Some(&a.use_header())).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(harness.channel(&a.id()).await.spent_amount, 50_000);
    *harness.controls.upstream_status.write().unwrap() = StatusCode::OK;

    for content_type in ["text/event-stream", "application/x-ndjson"] {
        *harness.controls.upstream_content_type.write().unwrap() = Some(content_type.into());
        rejected_without_charge(&harness, HOST_A, &a).await;
    }
    *harness.controls.upstream_content_type.write().unwrap() = None;

    *harness.controls.policy_status.write().unwrap() = StatusCode::SERVICE_UNAVAILABLE;
    rejected_without_charge(&harness, HOST_A, &a).await;
    *harness.controls.policy_status.write().unwrap() = StatusCode::OK;
    *harness.controls.metadata_expiry.write().unwrap() = now() - 1;
    rejected_without_charge(&harness, HOST_A, &a).await;
    *harness.controls.metadata_expiry.write().unwrap() = now() + 3600;

    // Rebuild both the Redis-backed template and the HTTP gateway; no channel
    // fixture is reinserted. Existing proofs and their exact debits survive.
    harness.reconnect().await;
    delivered(&harness, HOST_A, &a, 100_000).await;
    delivered(&harness, HOST_B, &b, 200_000).await;

    harness.controls.set_policy(
        HOST_A,
        policy(HOST_A, 75_000, &recipient_updated.to_string(), 2),
    );
    rejected_without_charge(&harness, HOST_A, &a).await;
    let updated = open(&harness, HOST_A, &payer_key, 75_000).await;
    harness.controls.policies.write().unwrap().remove(HOST_A);
    rejected_without_charge(&harness, HOST_A, &updated).await;
    let response = harness
        .client
        .get(format!("{}/.well-known/pay", harness.gateway.url))
        .header("host", HOST_A)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(harness.channel(&a.id()).await.spent_amount, 100_000);
    assert_eq!(harness.channel(&b.id()).await.spent_amount, 200_000);
    assert_eq!(harness.channel(&updated.id()).await.spent_amount, 75_000);

    let wallets = [
        recipients_a[0],
        recipients_a[1],
        recipients_a[2],
        recipient_b,
        recipient_updated,
    ];
    let mut before = Vec::new();
    for wallet in &wallets {
        before.push(token_balance(&harness.endpoints.rpc, wallet, mint).await);
    }
    let operator_before =
        token_balance(&harness.endpoints.rpc, &harness.operator.pubkey(), mint).await;
    let rpc_url = harness.endpoints.rpc.clone();
    let redis_url = harness.endpoints.redis.clone();
    let prefix = harness.redis_prefix.clone();
    let operator_key = harness.config.operator_signing_key.clone().unwrap();
    let ids = [a.id(), b.id(), updated.id()];
    let mut close_after = 0;
    for id in &ids {
        close_after = close_after.max(harness.channel(id).await.lifecycle.unwrap().close_after);
    }
    let wait_ms = close_after.saturating_sub(now() * 1000) + 100;
    assert!(
        wait_ms <= 10_000,
        "fixture idle deadline is {wait_ms}ms away"
    );
    // Stop the gateway and resolver before settlement. The worker must recover
    // solely from Redis plus chain state, not the current policy or its cache.
    harness.shutdown().await;
    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    tokio::join!(
        run_worker(&rpc_url, &redis_url, &prefix, &operator_key),
        run_worker(&rpc_url, &redis_url, &prefix, &operator_key),
    );
    let expected = [60_000, 30_000, 10_000, 200_000, 75_000];
    for ((wallet, before), delta) in wallets.iter().zip(&before).zip(expected) {
        assert_eq!(
            token_balance(&rpc_url, wallet, mint).await - before,
            delta,
            "original snapshot recipient payout: {wallet}",
        );
    }
    assert_eq!(
        token_balance(&rpc_url, &signer(&operator_key).pubkey(), mint).await,
        operator_before,
        "the operator must not retain the sellers' token allocation",
    );
    let rpc =
        pay_kit::mpp::solana_rpc_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
            rpc_url.clone(),
            solana_commitment_config::CommitmentConfig::confirmed(),
        );
    for id in ids {
        let data = rpc
            .get_account_data(&Pubkey::from_str(&id).unwrap())
            .await
            .unwrap();
        let channel =
            pay_kit::mpp::program::payment_channels::generated::accounts::Channel::from_bytes(
                &data,
            )
            .expect("real on-chain channel");
        assert_eq!(
            channel.status,
            pay_kit::mpp::program::payment_channels::generated::types::ChannelStatus::Distributed
                as u8,
            "channel must be sealed and its escrow distributed",
        );
    }
    assert_eq!(
        token_balance(&rpc_url, &payer.pubkey(), mint).await,
        20_000_000 - 375_000,
        "unused deposits return to the payer; only accepted requests are spent",
    );
    // A second process must not pay the same accepted vouchers again.
    run_worker(&rpc_url, &redis_url, &prefix, &operator_key).await;
    for ((wallet, before), delta) in wallets.iter().zip(&before).zip(expected) {
        assert_eq!(token_balance(&rpc_url, wallet, mint).await - before, delta);
    }
}
