//! Pull-session integration tests using surfpool-sdk.
//!
//! These tests use a real Surfnet runtime for the multi-delegator state machine
//! and real client-side pull-session header construction. Channel-open batching
//! is captured via the test batch submitter so assertions stay precise.

#![cfg(feature = "server")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::middleware;
use axum::response::Response;
use axum::routing::any;
use pay_core::PaymentState;
use pay_core::accounts::{Account, AccountsFile, Keystore, MemoryAccountsStore};
use pay_core::server::session::{MultiDelegateChain, RpcMultiDelegateChain, SessionMpp};
use pay_types::metering::ApiSpec;
use serial_test::serial;
use solana_mpp::client::multi_delegate::build_init_multi_delegate_tx;
use solana_mpp::program::multi_delegator::{MULTI_DELEGATOR_PROGRAM_ID, MultiDelegateOnChainState};
use solana_mpp::server::session::SessionConfig;
use solana_mpp::solana_keychain::memory::MemorySigner;
use solana_mpp::{
    PaymentChallenge, SessionAction, SessionMode, SessionRequest, parse_authorization,
    parse_www_authenticate,
};
use solana_pubkey::Pubkey;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use surfpool_sdk::{Keypair, Signer};
use tokio::time::{Duration, sleep};
use tower::util::ServiceExt;

type BatchLog = Arc<Mutex<Vec<Vec<(String, String, u64)>>>>;

const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TEST_DEPOSIT: u64 = 1_000_000;

#[derive(Clone)]
struct TestState {
    apis: Arc<Vec<ApiSpec>>,
    session_mpp: Arc<SessionMpp>,
}

impl PaymentState for TestState {
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

#[derive(Clone)]
struct RecordingRpcChain {
    inner: Arc<RpcMultiDelegateChain>,
    submitted: Arc<Mutex<Vec<String>>>,
}

impl RecordingRpcChain {
    fn new(inner: RpcMultiDelegateChain) -> Self {
        Self {
            inner: Arc::new(inner),
            submitted: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn submitted_txs(&self) -> Vec<String> {
        self.submitted.lock().unwrap().clone()
    }

    async fn fetch_state_now(&self, owner: &str) -> pay_core::Result<MultiDelegateOnChainState> {
        self.inner.fetch_state(owner).await
    }
}

impl MultiDelegateChain for RecordingRpcChain {
    fn fetch_state<'a>(
        &'a self,
        owner: &'a str,
    ) -> Pin<Box<dyn Future<Output = pay_core::Result<MultiDelegateOnChainState>> + Send + 'a>>
    {
        self.inner.fetch_state(owner)
    }

    fn submit_tx<'a>(
        &'a self,
        tx_base64: &'a str,
    ) -> Pin<Box<dyn Future<Output = pay_core::Result<String>> + Send + 'a>> {
        let submitted = Arc::clone(&self.submitted);
        let inner = Arc::clone(&self.inner);
        let tx = tx_base64.to_string();
        Box::pin(async move {
            submitted.lock().unwrap().push(tx.clone());
            inner.submit_tx(&tx).await
        })
    }
}

#[derive(Clone)]
struct StaticRecordingChain {
    state: MultiDelegateOnChainState,
    submitted: Arc<Mutex<Vec<String>>>,
}

impl StaticRecordingChain {
    fn new(state: MultiDelegateOnChainState) -> Self {
        Self {
            state,
            submitted: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn submitted_txs(&self) -> Vec<String> {
        self.submitted.lock().unwrap().clone()
    }
}

impl MultiDelegateChain for StaticRecordingChain {
    fn fetch_state<'a>(
        &'a self,
        _owner: &'a str,
    ) -> Pin<Box<dyn Future<Output = pay_core::Result<MultiDelegateOnChainState>> + Send + 'a>>
    {
        let state = self.state.clone();
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
            Ok("mock_update_sig".to_string())
        })
    }
}

#[derive(Debug, Clone)]
struct BuiltPullOpen {
    auth_header: String,
    owner: String,
    token_account: String,
    init_tx: String,
    update_tx: String,
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        // SAFETY: these tests are serialised and mutate process env only within
        // the guarded scope so `open_pull_session_header` resolves the Surfnet RPC.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: paired with `set` above; tests are serial.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

async fn start_surfnet() -> surfpool_sdk::Surfnet {
    surfpool_sdk::Surfnet::builder()
        .remote_rpc_url("https://api.mainnet-beta.solana.com")
        .airdrop_sol(10_000_000_000)
        .start()
        .await
        .expect("Failed to start Surfnet")
}

struct Cheatcodes {
    rpc_url: String,
}

impl Cheatcodes {
    fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
        }
    }

    fn fund_sol(&self, address: &Pubkey, lamports: u64) {
        let params = serde_json::json!([address.to_string(), { "lamports": lamports }]);
        self.call("surfnet_setAccount", params);
    }

    fn fund_token(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
        amount: u64,
        token_program: Option<&Pubkey>,
    ) {
        let token_program = token_program.copied().unwrap_or_else(default_token_program);
        let params = serde_json::json!([
            owner.to_string(),
            mint.to_string(),
            { "amount": amount },
            token_program.to_string()
        ]);
        self.call("surfnet_setTokenAccount", params);
    }

    fn get_ata(&self, owner: &Pubkey, mint: &Pubkey, token_program: Option<&Pubkey>) -> Pubkey {
        let token_program = token_program.copied().unwrap_or_else(default_token_program);
        let ata_program =
            Pubkey::from_str(solana_mpp::protocol::solana::programs::ASSOCIATED_TOKEN_PROGRAM)
                .unwrap();
        let (ata, _) = Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
            &ata_program,
        );
        ata
    }

    fn call(&self, method: &'static str, params: serde_json::Value) {
        use solana_client::rpc_request::RpcRequest;
        use solana_mpp::solana_rpc_client::rpc_client::RpcClient;

        RpcClient::new(self.rpc_url.to_string())
            .send::<serde_json::Value>(RpcRequest::Custom { method }, params)
            .unwrap_or_else(|e| panic!("Surfpool cheatcode {method} failed: {e}"));
    }
}

fn memory_store_for_keypair(keypair: &Keypair) -> MemoryAccountsStore {
    let mut file = AccountsFile::default();
    file.upsert(
        "localnet",
        "default",
        Account {
            keystore: Keystore::Ephemeral,
            active: false,
            auth_required: Some(false),
            pubkey: Some(keypair.pubkey().to_string()),
            vault: None,
            path: None,
            secret_key_b58: Some(bs58::encode(keypair.to_bytes()).into_string()),
            created_at: Some("2026-04-19T00:00:00Z".to_string()),
        },
    );
    MemoryAccountsStore::with_file(file)
}

fn usdc_mint() -> Pubkey {
    Pubkey::from_str(USDC_MINT).unwrap()
}

fn default_token_program() -> Pubkey {
    Pubkey::from_str(SPL_TOKEN_PROGRAM).unwrap()
}

fn multi_delegator_program() -> Pubkey {
    Pubkey::from_str(MULTI_DELEGATOR_PROGRAM_ID).unwrap()
}

fn load_api_fixture() -> ApiSpec {
    serde_yml::from_str(
        &std::fs::read_to_string("tests/fixtures/test-provider.yml")
            .expect("read test provider fixture"),
    )
    .expect("parse test provider fixture")
}

fn build_app(session_mpp: SessionMpp) -> Router {
    let state = TestState {
        apis: Arc::new(vec![load_api_fixture()]),
        session_mpp: Arc::new(session_mpp),
    };

    Router::new()
        .fallback(any(|| async {
            axum::Json(serde_json::json!({ "ok": true }))
        }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::payment::payment_middleware::<TestState>,
        ))
        .with_state(state)
}

async fn send_request(app: &Router, authorization: Option<&str>) -> Response {
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

async fn fetch_challenge(app: &Router) -> (PaymentChallenge, SessionRequest) {
    let resp = send_request(app, None).await;
    assert_eq!(resp.status(), 402);
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .expect("missing www-authenticate")
        .to_str()
        .unwrap();
    let challenge = parse_www_authenticate(www_auth).expect("parse session challenge");
    let request: SessionRequest = challenge.request.decode().expect("decode session request");
    assert!(
        request.recent_blockhash.is_some(),
        "session challenge should prefetch recentBlockhash",
    );
    (challenge, request)
}

async fn build_pull_open(
    rpc_url: &str,
    challenge: &PaymentChallenge,
    request: &SessionRequest,
    user: &Keypair,
    deposit: u64,
) -> BuiltPullOpen {
    let rpc_url = rpc_url.to_string();
    let challenge = challenge.clone();
    let request = request.clone();
    let user_bytes = user.to_bytes();
    let auth_header = tokio::task::spawn_blocking(move || {
        let _rpc_guard = EnvVarGuard::set("PAY_RPC_URL", &rpc_url);
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&user_bytes[..32]);
        let user = Keypair::new_from_array(secret);
        let store = memory_store_for_keypair(&user);
        let (_handle, auth_header) = pay_core::session::open_pull_session_header(
            &challenge,
            &request,
            &store,
            Some("localnet"),
            None,
            deposit,
            false,
        )
        .expect("build pull session header");
        auth_header
    })
    .await
    .expect("join pull open builder");

    let credential = parse_authorization(&auth_header).expect("parse authorization");
    let action: SessionAction =
        serde_json::from_value(credential.payload).expect("decode session action");
    let payload = match action {
        SessionAction::Open(payload) => payload,
        other => panic!("expected open payload, got {other:?}"),
    };

    assert_eq!(payload.mode, SessionMode::Pull);
    let expected_owner = user.pubkey().to_string();
    assert_eq!(payload.owner.as_deref(), Some(expected_owner.as_str()));

    BuiltPullOpen {
        auth_header,
        owner: payload.owner.expect("pull open owner"),
        token_account: payload.token_account.expect("pull open tokenAccount"),
        init_tx: payload
            .init_multi_delegate_tx
            .expect("pull open initMultiDelegateTx"),
        update_tx: payload
            .update_delegation_tx
            .expect("pull open updateDelegationTx"),
    }
}

async fn send_open_and_assert_ok(app: &Router, built: &BuiltPullOpen) {
    let resp = send_request(app, Some(&built.auth_header)).await;
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read response body");
    assert_eq!(
        status,
        200,
        "pull open should return 200, got {status}: {}",
        String::from_utf8_lossy(&body)
    );
}

async fn submit_base64_transaction(rpc_url: &str, tx_base64: &str) -> String {
    let rpc_url = rpc_url.to_string();
    let tx_base64 = tx_base64.to_string();
    tokio::task::spawn_blocking(move || {
        use base64::Engine;
        use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
        use solana_transaction::Transaction;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(tx_base64)
            .expect("decode tx base64");
        let tx: Transaction = bincode::deserialize(&bytes).expect("deserialize tx");
        RpcClient::new(rpc_url)
            .send_and_confirm_transaction(&tx)
            .expect("submit tx")
            .to_string()
    })
    .await
    .expect("join submit tx task")
}

async fn seed_delegation_cap(
    cheatcodes: &Cheatcodes,
    rpc_url: &str,
    user: &Keypair,
    operator: &Keypair,
    cap: u64,
) -> String {
    use solana_mpp::solana_rpc_client::rpc_client::RpcClient;

    let user_signer = MemorySigner::from_bytes(&user.to_bytes()).expect("user signer");
    let recent_blockhash = RpcClient::new(rpc_url.to_string())
        .get_latest_blockhash()
        .expect("latest blockhash");
    let user_ata = cheatcodes.get_ata(&user.pubkey(), &usdc_mint(), Some(&default_token_program()));
    let tx = build_init_multi_delegate_tx(
        &user_signer,
        &usdc_mint(),
        &user_ata,
        &operator.pubkey(),
        &multi_delegator_program(),
        &default_token_program(),
        0,
        cap,
        9_999_999_999,
        recent_blockhash,
    )
    .await
    .expect("build init delegation tx");
    submit_base64_transaction(rpc_url, &tx).await;
    user_ata.to_string()
}

fn new_recording_chain(rpc_url: &str, operator: &Keypair) -> RecordingRpcChain {
    RecordingRpcChain::new(RpcMultiDelegateChain {
        rpc_url: rpc_url.to_string(),
        program_id: multi_delegator_program(),
        mint: usdc_mint(),
        operator: operator.pubkey(),
        delegation_nonce: 0,
    })
}

fn sorted_batch(batch: &[(String, String, u64)]) -> Vec<(String, String, u64)> {
    let mut batch = batch.to_vec();
    batch.sort();
    batch
}

async fn make_session_app(
    chain: Box<dyn MultiDelegateChain>,
    batches: BatchLog,
    operator: &Keypair,
    recipient: &Keypair,
    rpc_url: &str,
    batch_interval_ms: u64,
) -> Router {
    let session_mpp = SessionMpp::new(
        SessionConfig {
            operator: operator.pubkey().to_string(),
            recipient: recipient.pubkey().to_string(),
            max_cap: 5_000_000,
            currency: USDC_MINT.to_string(),
            decimals: 6,
            network: "localnet".to_string(),
            modes: vec![SessionMode::Pull],
            rpc_url: Some(rpc_url.to_string()),
            ..Default::default()
        },
        "test-session-secret",
    )
    .with_multi_delegate_chain(chain)
    .with_test_open_channel_batcher_interval(batch_interval_ms, {
        let batches = Arc::clone(&batches);
        move |batch| {
            let batches = Arc::clone(&batches);
            async move {
                batches.lock().unwrap().push(batch);
                Ok(())
            }
        }
    });

    build_app(session_mpp)
}

fn fund_participant(cheatcodes: &Cheatcodes, keypair: &Keypair, usdc_amount: u64) {
    cheatcodes.fund_sol(&keypair.pubkey(), 2_000_000_000);
    cheatcodes.fund_token(
        &keypair.pubkey(),
        &usdc_mint(),
        usdc_amount,
        Some(&default_token_program()),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn pull_session_surfpool_init_path_submits_init_and_batches_open() {
    let surfnet = start_surfnet().await;
    let rpc_url = surfnet.rpc_url().to_string();
    let cheatcodes = Cheatcodes::new(&rpc_url);
    let operator = Keypair::new();
    let recipient = Keypair::new();
    let user = Keypair::new();
    fund_participant(&cheatcodes, &operator, 0);
    fund_participant(&cheatcodes, &recipient, 0);
    fund_participant(&cheatcodes, &user, 5_000_000);

    let batches: BatchLog = Arc::new(Mutex::new(Vec::new()));
    let chain = new_recording_chain(&rpc_url, &operator);
    let app = make_session_app(
        Box::new(chain.clone()),
        Arc::clone(&batches),
        &operator,
        &recipient,
        &rpc_url,
        400,
    )
    .await;

    let (challenge, request) = fetch_challenge(&app).await;
    let built = build_pull_open(&rpc_url, &challenge, &request, &user, TEST_DEPOSIT).await;
    send_open_and_assert_ok(&app, &built).await;
    sleep(Duration::from_millis(550)).await;

    assert_eq!(chain.submitted_txs(), vec![built.init_tx.clone()]);
    let state = chain
        .fetch_state_now(&user.pubkey().to_string())
        .await
        .expect("fetch on-chain state");
    assert!(state.multi_delegate_exists);
    assert_eq!(state.existing_delegation_cap, Some(TEST_DEPOSIT));

    let batches = batches.lock().unwrap().clone();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0],
        vec![(
            built.owner.clone(),
            built.token_account.clone(),
            TEST_DEPOSIT
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn pull_session_surfpool_update_path_submits_update_and_batches_open() {
    let surfnet = start_surfnet().await;
    let rpc_url = surfnet.rpc_url().to_string();
    let cheatcodes = Cheatcodes::new(&rpc_url);
    let operator = Keypair::new();
    let recipient = Keypair::new();
    let user = Keypair::new();
    fund_participant(&cheatcodes, &operator, 0);
    fund_participant(&cheatcodes, &recipient, 0);
    fund_participant(&cheatcodes, &user, 5_000_000);
    let batches: BatchLog = Arc::new(Mutex::new(Vec::new()));
    let chain = StaticRecordingChain::new(MultiDelegateOnChainState {
        multi_delegate_exists: true,
        existing_delegation_cap: Some(250_000),
    });
    let app = make_session_app(
        Box::new(chain.clone()),
        Arc::clone(&batches),
        &operator,
        &recipient,
        &rpc_url,
        400,
    )
    .await;

    let (challenge, request) = fetch_challenge(&app).await;
    let built = build_pull_open(&rpc_url, &challenge, &request, &user, TEST_DEPOSIT).await;
    send_open_and_assert_ok(&app, &built).await;
    sleep(Duration::from_millis(550)).await;

    assert_eq!(chain.submitted_txs(), vec![built.update_tx.clone()]);

    let batches = batches.lock().unwrap().clone();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0],
        vec![(
            built.owner.clone(),
            built.token_account.clone(),
            TEST_DEPOSIT
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn pull_session_surfpool_sufficient_path_submits_no_setup_tx_and_batches_open() {
    let surfnet = start_surfnet().await;
    let rpc_url = surfnet.rpc_url().to_string();
    let cheatcodes = Cheatcodes::new(&rpc_url);
    let operator = Keypair::new();
    let recipient = Keypair::new();
    let user = Keypair::new();
    fund_participant(&cheatcodes, &operator, 0);
    fund_participant(&cheatcodes, &recipient, 0);
    fund_participant(&cheatcodes, &user, 5_000_000);
    let expected_token_account =
        seed_delegation_cap(&cheatcodes, &rpc_url, &user, &operator, TEST_DEPOSIT).await;

    let batches: BatchLog = Arc::new(Mutex::new(Vec::new()));
    let chain = new_recording_chain(&rpc_url, &operator);
    let app = make_session_app(
        Box::new(chain.clone()),
        Arc::clone(&batches),
        &operator,
        &recipient,
        &rpc_url,
        400,
    )
    .await;

    let (challenge, request) = fetch_challenge(&app).await;
    let built = build_pull_open(&rpc_url, &challenge, &request, &user, TEST_DEPOSIT).await;
    assert_eq!(built.token_account, expected_token_account);
    send_open_and_assert_ok(&app, &built).await;
    sleep(Duration::from_millis(550)).await;

    assert!(
        chain.submitted_txs().is_empty(),
        "server should not submit init/update when delegation is already sufficient"
    );
    let state = chain
        .fetch_state_now(&user.pubkey().to_string())
        .await
        .expect("fetch unchanged state");
    assert!(state.multi_delegate_exists);
    assert_eq!(state.existing_delegation_cap, Some(TEST_DEPOSIT));

    let batches = batches.lock().unwrap().clone();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0],
        vec![(
            built.owner.clone(),
            built.token_account.clone(),
            TEST_DEPOSIT
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn pull_session_surfpool_multi_account_opens_share_one_batch() {
    let surfnet = start_surfnet().await;
    let rpc_url = surfnet.rpc_url().to_string();
    let cheatcodes = Cheatcodes::new(&rpc_url);
    let operator = Keypair::new();
    let recipient = Keypair::new();
    let alice = Keypair::new();
    let bob = Keypair::new();
    fund_participant(&cheatcodes, &operator, 0);
    fund_participant(&cheatcodes, &recipient, 0);
    fund_participant(&cheatcodes, &alice, 5_000_000);
    fund_participant(&cheatcodes, &bob, 5_000_000);
    seed_delegation_cap(&cheatcodes, &rpc_url, &alice, &operator, TEST_DEPOSIT).await;
    seed_delegation_cap(&cheatcodes, &rpc_url, &bob, &operator, TEST_DEPOSIT).await;

    let batches: BatchLog = Arc::new(Mutex::new(Vec::new()));
    let chain = new_recording_chain(&rpc_url, &operator);
    let app = make_session_app(
        Box::new(chain.clone()),
        Arc::clone(&batches),
        &operator,
        &recipient,
        &rpc_url,
        5_000,
    )
    .await;

    let (challenge, request) = fetch_challenge(&app).await;
    let alice_open = build_pull_open(&rpc_url, &challenge, &request, &alice, TEST_DEPOSIT).await;
    let bob_open = build_pull_open(&rpc_url, &challenge, &request, &bob, TEST_DEPOSIT).await;

    send_open_and_assert_ok(&app, &alice_open).await;
    send_open_and_assert_ok(&app, &bob_open).await;
    sleep(Duration::from_millis(5_250)).await;

    assert!(
        chain.submitted_txs().is_empty(),
        "pre-seeded accounts should hit the no-setup path so only batching is exercised"
    );
    let batches = batches.lock().unwrap().clone();
    assert_eq!(batches.len(), 1, "expected one batch flush");
    assert_eq!(batches[0].len(), 2, "expected both opens in the same batch");
    assert_eq!(
        sorted_batch(&batches[0]),
        sorted_batch(&[
            (
                alice_open.owner.clone(),
                alice_open.token_account.clone(),
                TEST_DEPOSIT,
            ),
            (
                bob_open.owner.clone(),
                bob_open.token_account.clone(),
                TEST_DEPOSIT,
            ),
        ])
    );
}
