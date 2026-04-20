//! Session intent client — open channels, sign vouchers, close.
//!
//! A session keeps a pre-funded on-chain Fiber channel open across many API
//! calls. Each call consumes a small voucher increment instead of a full
//! on-chain transaction, making high-frequency AI workloads cheap.
//!
//! # Lifecycle
//!
//! ```text
//! 1. Server returns 402 with session challenge (intent="session")
//! 2. Client creates a Fiber channel on-chain → gets channel_id + tx_sig
//! 3. Client calls SessionHandle::new() and sends open_header() on first request
//! 4. For each subsequent request: voucher_header(cost_per_request)
//! 5. When done: close_header() triggers on-chain settlement
//! ```

use std::sync::Arc;

use solana_mpp::client::session::ActiveSession;
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::{
    PaymentChallenge, PaymentCredential, SessionAction, SessionRequest, format_authorization,
    parse_www_authenticate,
};
use solana_pubkey::Pubkey;
use tokio::sync::Mutex;

use crate::{Error, Result};

// Re-export so callers can construct their own sessions without depending on
// solana_mpp directly.
pub use solana_mpp::client::session::ActiveSession as RawSession;

/// A live session: wraps an [`ActiveSession`] and the original challenge so
/// voucher authorization headers can be produced without re-parsing the
/// challenge on each call.
///
/// `SessionHandle` is `Clone` and `Send + Sync` — safe to share across async
/// tasks (e.g., a middleware that reuses the same channel for all in-flight
/// requests to the same server).
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<Mutex<ActiveSession>>,
    /// Original challenge — echoed back in every `PaymentCredential`.
    challenge: PaymentChallenge,
}

impl SessionHandle {
    /// Try to parse a session challenge from a `WWW-Authenticate` header value.
    ///
    /// Returns `None` if the header is absent, uses a different scheme, or
    /// carries a non-session intent.
    pub fn parse_challenge(header: &str) -> Option<(PaymentChallenge, SessionRequest)> {
        let challenge = parse_www_authenticate(header).ok()?;
        if challenge.intent.as_str() != "session" {
            return None;
        }
        let request: SessionRequest = challenge.request.decode().ok()?;
        Some((challenge, request))
    }

    /// Create a handle wrapping an already-opened channel.
    ///
    /// `channel_id` is the on-chain Fiber channel public key — obtained after
    /// broadcasting and confirming the open transaction.
    /// `signer` is the session key whose public key was passed as
    /// `authorized_signer` in the open transaction.
    pub fn new(
        channel_id: Pubkey,
        signer: Box<dyn SolanaSigner>,
        challenge: PaymentChallenge,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ActiveSession::new(channel_id, signer))),
            challenge,
        }
    }

    /// Build an `Authorization` header for the `open` action.
    ///
    /// Send this on the **first** request after the on-chain open transaction
    /// has been confirmed.
    ///
    /// * `deposit` — amount locked on-chain (base units, e.g. µUSDC)
    /// * `open_tx_signature` — base58 Solana transaction signature
    pub async fn open_header(&self, deposit: u64, open_tx_signature: &str) -> Result<String> {
        let session = self.inner.lock().await;
        let action = session.open_action(deposit, open_tx_signature);
        build_header(&self.challenge, &action)
    }

    /// Build an `Authorization` header carrying a voucher for `amount` base units.
    ///
    /// Increments the cumulative watermark by `amount`. Call this before every
    /// metered API request (after the initial open).
    pub async fn voucher_header(&self, amount: u64) -> Result<String> {
        let mut session = self.inner.lock().await;
        let action = session
            .voucher_action(amount)
            .await
            .map_err(|e| Error::Mpp(format!("Failed to sign voucher: {e}")))?;
        build_header(&self.challenge, &action)
    }

    /// Build an `Authorization` header for cooperative channel close.
    ///
    /// `final_increment` optionally adds a last voucher for any outstanding
    /// balance before close. Pass `None` if the channel is already fully
    /// settled.
    pub async fn close_header(&self, final_increment: Option<u64>) -> Result<String> {
        let mut session = self.inner.lock().await;
        let action = session
            .close_action(final_increment)
            .await
            .map_err(|e| Error::Mpp(format!("Failed to build close action: {e}")))?;
        build_header(&self.challenge, &action)
    }

    /// Build an `Authorization` header for a pull-mode `open` action.
    ///
    /// The two pre-signed delegation transactions (`init_tx`, `update_tx`) are
    /// built by [`open_pull_session_header`] and attached here. The server will
    /// submit whichever transaction is appropriate for the current on-chain state.
    pub async fn open_pull_header(
        &self,
        approved_amount: u64,
        owner: &str,
        approve_sig: &str,
        init_tx: String,
        update_tx: String,
    ) -> Result<String> {
        use solana_mpp::SessionAction;
        let session = self.inner.lock().await;
        let SessionAction::Open(payload) =
            session.open_pull_action(approved_amount, owner, approve_sig)
        else {
            unreachable!("open_pull_action always returns SessionAction::Open")
        };
        let payload = payload.with_init_tx(init_tx).with_update_tx(update_tx);
        build_header(&self.challenge, &SessionAction::Open(payload))
    }

    /// Build an `Authorization` header for a top-up after adding more funds
    /// on-chain.
    ///
    /// * `new_deposit` — new total deposit after the top-up (base units)
    /// * `topup_tx_signature` — base58 Solana transaction signature
    pub async fn topup_header(&self, new_deposit: u64, topup_tx_signature: &str) -> Result<String> {
        let session = self.inner.lock().await;
        let action = session.topup_action(new_deposit, topup_tx_signature);
        build_header(&self.challenge, &action)
    }

    /// Current cumulative amount authorized so far (base units).
    pub async fn cumulative(&self) -> u64 {
        self.inner.lock().await.cumulative
    }

    /// Channel ID as base58 (matches what was registered with the server).
    pub async fn channel_id(&self) -> String {
        self.inner.lock().await.channel_id_str()
    }

    /// The original server challenge — useful for logging or re-use.
    pub fn challenge(&self) -> &PaymentChallenge {
        &self.challenge
    }
}

// ── One-shot session pay ──────────────────────────────────────────────────────

/// Make a single API call through a session-gated endpoint.
///
/// Creates an ephemeral keypair, opens a session with the given `deposit`
/// (base units), sends the `open` action as the Authorization header, and
/// returns the `Authorization` header value to use for the retry.
///
/// The server currently trusts the deposit without on-chain verification,
/// so this works without a real Fiber channel for development/testing.
pub fn open_session_header(
    challenge: &PaymentChallenge,
    deposit: u64,
) -> Result<(SessionHandle, String)> {
    use ed25519_dalek::SigningKey;
    use solana_mpp::solana_keychain::MemorySigner;
    use solana_pubkey::Pubkey;

    // Generate a fresh ephemeral session keypair.
    let sk = SigningKey::generate(&mut rand::thread_rng());
    let vk = sk.verifying_key();
    let mut kp = [0u8; 64];
    kp[..32].copy_from_slice(sk.as_bytes());
    kp[32..].copy_from_slice(vk.as_bytes());
    let signer: Box<dyn solana_mpp::solana_keychain::SolanaSigner> =
        Box::new(MemorySigner::from_bytes(&kp).map_err(|e| Error::Mpp(e.to_string()))?);

    // Random channel ID — server stores it keyed by this string.
    let channel_id = Pubkey::new_unique();

    let handle = SessionHandle::new(channel_id, signer, challenge.clone());

    // Build the open header (fake tx sig — server trusts it for now).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to build runtime: {e}")))?;
    let auth_header = rt.block_on(handle.open_header(deposit, "demo_open_tx"))?;

    Ok((handle, auth_header))
}

/// Open a pull-mode session: loads the user's wallet, derives the USDC ATA,
/// builds both delegation transactions (init + update), and returns an
/// `Authorization` header carrying the `open` action with both txs attached.
///
/// # Parameters
/// - `challenge` — the 402 session challenge from the server
/// - `request` — the decoded `SessionRequest` (contains operator pubkey, mint, etc.)
/// - `store` — accounts store used to load the user's signing keypair
/// - `network_override` — `Some("localnet")` for `--sandbox`, `None` to trust challenge
/// - `deposit` — amount to approve (µUSDC)
/// - `sandbox` — when `true`, auto-funds the wallet via Surfpool before building txs
pub fn open_pull_session_header(
    challenge: &PaymentChallenge,
    request: &solana_mpp::SessionRequest,
    store: &dyn crate::accounts::AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
    deposit: u64,
    sandbox: bool,
) -> Result<(SessionHandle, String)> {
    use solana_hash::Hash;
    use solana_mpp::client::multi_delegate::{
        build_init_multi_delegate_tx, build_update_delegation_tx,
    };
    use solana_mpp::program::multi_delegator::MULTI_DELEGATOR_PROGRAM_ID;
    use solana_mpp::protocol::solana::{default_rpc_url, programs};
    use solana_mpp::solana_keychain::MemorySigner;
    use solana_pubkey::Pubkey;
    use std::str::FromStr;

    let network = network_override.map(str::to_string).unwrap_or_else(|| {
        request
            .network
            .clone()
            .unwrap_or_else(|| "mainnet".to_string())
    });

    // Load the user's wallet keypair
    let (signer, ephemeral_notice) = crate::signer::load_signer_for_network_with_reason(
        &network,
        store,
        account_override,
        "authorize payment",
    )?;
    let user_pubkey = signer.pubkey();

    // Resolve RPC endpoint
    let rpc_url =
        std::env::var("PAY_RPC_URL").unwrap_or_else(|_| default_rpc_url(&network).to_string());

    // Operator pubkey (delegatee in every FixedDelegation)
    let operator_pk = Pubkey::from_str(&request.operator)
        .map_err(|_| Error::Mpp(format!("invalid operator pubkey: {}", request.operator)))?;

    // Mint and token program (currency field carries the resolved mint address)
    let mint_pk = Pubkey::from_str(&request.currency).map_err(|_| {
        Error::Mpp(format!(
            "invalid mint address in challenge: {}",
            request.currency
        ))
    })?;
    let token_program_pk = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();

    // Derive the user's ATA: find_program_address([owner, token_program, mint], ata_program)
    let ata_program_pk = Pubkey::from_str(programs::ASSOCIATED_TOKEN_PROGRAM).unwrap();
    let (user_ata, _) = Pubkey::find_program_address(
        &[
            user_pubkey.as_ref(),
            token_program_pk.as_ref(),
            mint_pk.as_ref(),
        ],
        &ata_program_pk,
    );

    let program_id_pk = Pubkey::from_str(MULTI_DELEGATOR_PROGRAM_ID).unwrap();

    tracing::info!(
        user = %user_pubkey,
        operator = %operator_pk,
        mint = %mint_pk,
        token_account = %user_ata,
        deposit,
        network,
        "building pull-mode session payloads"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create async runtime: {e}")))?;

    // Step 1 (optional): auto-fund user wallet in sandbox mode
    if sandbox && ephemeral_notice.is_some() {
        let pubkey = user_pubkey.to_string();
        let rpc = rpc_url.clone();
        if let Err(e) = rt.block_on(crate::client::sandbox::fund_via_surfpool(&rpc, &pubkey)) {
            tracing::warn!(error = %e, "Surfpool auto-fund failed — USDC balance may be 0");
        }
    }

    // Prefer the server-provided blockhash to avoid a redundant client-side
    // RPC call. Fall back to the cluster only when the challenge omitted it.
    let recent_blockhash = if let Some(blockhash) = request.recent_blockhash.as_deref() {
        Hash::from_str(blockhash)
            .map_err(|e| Error::Mpp(format!("invalid recentBlockhash in challenge: {e}")))?
    } else {
        use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
        RpcClient::new(rpc_url.clone())
            .get_latest_blockhash()
            .map_err(|e| Error::Mpp(format!("failed to get recent blockhash: {e}")))?
    };

    // Step 3: build both delegation transactions (async signers)
    let expiry_ts = 9_999_999_999i64; // far-future expiry

    let (init_tx_b64, update_tx_b64) = rt.block_on(async {
        let signer_ref: &dyn solana_mpp::solana_keychain::SolanaSigner = &signer;
        let init = build_init_multi_delegate_tx(
            signer_ref,
            &mint_pk,
            &user_ata,
            &operator_pk,
            &program_id_pk,
            &token_program_pk,
            0, // nonce
            deposit,
            expiry_ts,
            recent_blockhash,
        )
        .await
        .map_err(|e| Error::Mpp(format!("build_init_multi_delegate_tx: {e}")))?;

        let update = build_update_delegation_tx(
            signer_ref,
            &mint_pk,
            &operator_pk,
            &program_id_pk,
            0, // nonce
            deposit,
            expiry_ts,
            recent_blockhash,
        )
        .await
        .map_err(|e| Error::Mpp(format!("build_update_delegation_tx: {e}")))?;

        Ok::<_, Error>((init, update))
    })?;

    tracing::info!(
        init_tx_preview = %&init_tx_b64[..40.min(init_tx_b64.len())],
        update_tx_preview = %&update_tx_b64[..40.min(update_tx_b64.len())],
        "built pull-mode delegation transactions"
    );

    // Step 4: build session handle with a fresh ephemeral session keypair
    let sk = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
    let vk = sk.verifying_key();
    let mut kp_bytes = [0u8; 64];
    kp_bytes[..32].copy_from_slice(sk.as_bytes());
    kp_bytes[32..].copy_from_slice(vk.as_bytes());
    let session_signer: Box<dyn solana_mpp::solana_keychain::SolanaSigner> =
        Box::new(MemorySigner::from_bytes(&kp_bytes).map_err(|e| Error::Mpp(e.to_string()))?);

    // For pull-mode, the channel_id IS the user's token account
    let handle = SessionHandle::new(user_ata, session_signer, challenge.clone());

    let auth_header = rt.block_on(handle.open_pull_header(
        deposit,
        &user_pubkey.to_string(),
        "pull_delegation_setup",
        init_tx_b64,
        update_tx_b64,
    ))?;

    tracing::info!(
        user = %user_pubkey,
        token_account = %user_ata,
        deposit,
        "pull-mode session authorization header ready"
    );

    Ok((handle, auth_header))
}

/// Build a voucher header for a subsequent call on an open session.
pub fn voucher_header_sync(handle: &SessionHandle, amount: u64) -> Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to build runtime: {e}")))?;
    rt.block_on(handle.voucher_header(amount))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_header(challenge: &PaymentChallenge, action: &SessionAction) -> Result<String> {
    let credential = PaymentCredential::new(challenge.to_echo(), action);
    format_authorization(&credential)
        .map_err(|e| Error::Mpp(format!("Failed to format authorization header: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Account, AccountsFile, Keystore, MemoryAccountsStore};
    use serial_test::serial;
    use solana_mpp::{Base64UrlJson, SessionMode, SessionSplit, parse_authorization};
    use surfpool_sdk::{Keypair, Signer};

    fn test_request() -> SessionRequest {
        SessionRequest {
            cap: "1000000".to_string(),
            currency: solana_pubkey::Pubkey::new_unique().to_string(),
            decimals: Some(6),
            network: Some("localnet".to_string()),
            operator: solana_pubkey::Pubkey::new_unique().to_string(),
            recipient: solana_pubkey::Pubkey::new_unique().to_string(),
            splits: vec![SessionSplit {
                recipient: solana_pubkey::Pubkey::new_unique().to_string(),
                amount: "100".to_string(),
            }],
            program_id: Some(solana_pubkey::Pubkey::new_unique().to_string()),
            description: Some("test session".to_string()),
            external_id: Some("ext-123".to_string()),
            min_voucher_delta: Some("25".to_string()),
            modes: vec![SessionMode::Push, SessionMode::Pull],
            recent_blockhash: None,
        }
    }

    fn test_challenge(intent: &str) -> PaymentChallenge {
        let request = Base64UrlJson::from_typed(&test_request()).unwrap();
        PaymentChallenge::with_secret_key("test-secret", "test-realm", "solana", intent, request)
    }

    fn test_signer() -> Box<dyn SolanaSigner> {
        use ed25519_dalek::SigningKey;
        use solana_mpp::solana_keychain::MemorySigner;

        let sk = SigningKey::generate(&mut rand::thread_rng());
        let vk = sk.verifying_key();
        let mut kp = [0u8; 64];
        kp[..32].copy_from_slice(sk.as_bytes());
        kp[32..].copy_from_slice(vk.as_bytes());
        Box::new(MemorySigner::from_bytes(&kp).unwrap())
    }

    fn parse_action(header: &str) -> SessionAction {
        let credential = parse_authorization(header).expect("parse authorization");
        serde_json::from_value(credential.payload).expect("decode session action")
    }

    fn memory_store_for_keypair(keypair: &Keypair) -> MemoryAccountsStore {
        let mut file = AccountsFile::default();
        file.upsert(
            "localnet",
            "default",
            Account {
                keystore: Keystore::Ephemeral,
                active: false,
                pubkey: Some(keypair.pubkey().to_string()),
                vault: None,
                path: None,
                secret_key_b58: Some(bs58::encode(keypair.to_bytes()).into_string()),
                created_at: Some("2026-04-19T00:00:00Z".to_string()),
            },
        );
        MemoryAccountsStore::with_file(file)
    }

    #[test]
    fn parse_challenge_only_accepts_session_headers() {
        let challenge = test_challenge("session");
        let header = challenge.to_header().unwrap();

        let Some((parsed_challenge, request)) = SessionHandle::parse_challenge(&header) else {
            panic!("expected a session challenge");
        };
        assert_eq!(parsed_challenge.intent.as_str(), "session");
        assert_eq!(request.cap, "1000000");

        let non_session = test_challenge("charge").to_header().unwrap();
        assert!(SessionHandle::parse_challenge(&non_session).is_none());
        assert!(SessionHandle::parse_challenge("not a challenge").is_none());
    }

    #[tokio::test]
    async fn session_handle_builds_expected_headers() {
        let channel_id = Pubkey::new_unique();
        let channel_id_str = channel_id.to_string();
        let challenge = test_challenge("session");
        let handle = SessionHandle::new(channel_id, test_signer(), challenge.clone());

        let open = parse_action(&handle.open_header(1_000_000, "open_sig").await.unwrap());
        match open {
            SessionAction::Open(payload) => {
                assert_eq!(payload.mode, SessionMode::Push);
                assert_eq!(payload.channel_id.as_deref(), Some(channel_id_str.as_str()));
                assert_eq!(payload.deposit.as_deref(), Some("1000000"));
                assert_eq!(payload.signature, "open_sig");
            }
            _ => panic!("expected open action"),
        }

        let voucher = parse_action(&handle.voucher_header(125).await.unwrap());
        match voucher {
            SessionAction::Voucher(payload) => {
                assert_eq!(payload.voucher.data.channel_id, channel_id_str);
                assert_eq!(payload.voucher.data.cumulative, "125");
            }
            _ => panic!("expected voucher action"),
        }
        assert_eq!(handle.cumulative().await, 125);
        assert_eq!(handle.channel_id().await, channel_id.to_string());
        assert_eq!(handle.challenge().intent, challenge.intent);

        let topup = parse_action(&handle.topup_header(2_000_000, "topup_sig").await.unwrap());
        match topup {
            SessionAction::TopUp(payload) => {
                assert_eq!(payload.channel_id, channel_id.to_string());
                assert_eq!(payload.new_deposit, "2000000");
                assert_eq!(payload.signature, "topup_sig");
            }
            _ => panic!("expected topup action"),
        }

        let close = parse_action(&handle.close_header(Some(25)).await.unwrap());
        match close {
            SessionAction::Close(payload) => {
                let voucher = payload.voucher.expect("final voucher");
                assert_eq!(voucher.data.cumulative, "150");
            }
            _ => panic!("expected close action"),
        }
    }

    #[tokio::test]
    async fn open_pull_header_attaches_both_delegation_payloads() {
        let token_account = Pubkey::new_unique();
        let token_account_str = token_account.to_string();
        let owner = Pubkey::new_unique().to_string();
        let handle = SessionHandle::new(token_account, test_signer(), test_challenge("session"));

        let action = parse_action(
            &handle
                .open_pull_header(
                    1_000_000,
                    &owner,
                    "approve_sig",
                    "init_tx_b64".to_string(),
                    "update_tx_b64".to_string(),
                )
                .await
                .unwrap(),
        );
        match action {
            SessionAction::Open(payload) => {
                assert_eq!(payload.mode, SessionMode::Pull);
                assert_eq!(
                    payload.token_account.as_deref(),
                    Some(token_account_str.as_str())
                );
                assert_eq!(payload.approved_amount.as_deref(), Some("1000000"));
                assert_eq!(payload.owner.as_deref(), Some(owner.as_str()));
                assert_eq!(
                    payload.init_multi_delegate_tx.as_deref(),
                    Some("init_tx_b64")
                );
                assert_eq!(
                    payload.update_delegation_tx.as_deref(),
                    Some("update_tx_b64")
                );
            }
            _ => panic!("expected pull open action"),
        }
    }

    #[test]
    fn open_session_header_returns_parseable_header() {
        let challenge = test_challenge("session");
        let (handle, header) = open_session_header(&challenge, 1_000_000).unwrap();
        let action = parse_action(&header);
        match action {
            SessionAction::Open(payload) => {
                assert_eq!(payload.mode, SessionMode::Push);
                assert_eq!(payload.deposit.as_deref(), Some("1000000"));
            }
            _ => panic!("expected open action"),
        }
        let parsed = SessionHandle::parse_challenge(&challenge.to_header().unwrap()).unwrap();
        assert_eq!(parsed.0.intent, handle.challenge().intent);
    }

    #[test]
    fn voucher_header_sync_matches_async_builder() {
        let handle = SessionHandle::new(
            Pubkey::new_unique(),
            test_signer(),
            test_challenge("session"),
        );
        let sync = voucher_header_sync(&handle, 42).unwrap();
        let action = parse_action(&sync);
        match action {
            SessionAction::Voucher(payload) => {
                assert_eq!(payload.voucher.data.cumulative, "42");
            }
            _ => panic!("expected voucher action"),
        }
    }

    #[test]
    #[serial]
    fn open_pull_session_header_rejects_invalid_operator() {
        let user = Keypair::new();
        let store = memory_store_for_keypair(&user);
        let mut request = test_request();
        request.operator = "not-a-pubkey".to_string();

        let err = open_pull_session_header(
            &test_challenge("session"),
            &request,
            &store,
            Some("localnet"),
            None,
            1_000_000,
            false,
        )
        .err()
        .expect("invalid operator should error");

        assert!(
            err.to_string().contains("invalid operator pubkey"),
            "got: {err}"
        );
    }

    #[test]
    #[serial]
    fn open_pull_session_header_rejects_invalid_mint() {
        let user = Keypair::new();
        let store = memory_store_for_keypair(&user);
        let mut request = test_request();
        request.currency = "not-a-mint".to_string();

        let err = open_pull_session_header(
            &test_challenge("session"),
            &request,
            &store,
            Some("localnet"),
            None,
            1_000_000,
            false,
        )
        .err()
        .expect("invalid mint should error");

        assert!(
            err.to_string().contains("invalid mint address"),
            "got: {err}"
        );
    }

    #[test]
    #[serial]
    fn open_pull_session_header_reports_rpc_failures() {
        let user = Keypair::new();
        let store = memory_store_for_keypair(&user);
        let original = std::env::var("PAY_RPC_URL").ok();
        // SAFETY: this test is `serial`, so no concurrent env access occurs.
        unsafe { std::env::set_var("PAY_RPC_URL", "http://127.0.0.1:1") };

        let err = open_pull_session_header(
            &test_challenge("session"),
            &test_request(),
            &store,
            Some("localnet"),
            None,
            1_000_000,
            false,
        )
        .err()
        .expect("rpc lookup should fail");

        match original {
            // SAFETY: this test is `serial`, so no concurrent env access occurs.
            Some(value) => unsafe { std::env::set_var("PAY_RPC_URL", value) },
            // SAFETY: this test is `serial`, so no concurrent env access occurs.
            None => unsafe { std::env::remove_var("PAY_RPC_URL") },
        }

        assert!(
            err.to_string().contains("failed to get recent blockhash"),
            "got: {err}"
        );
    }
}
