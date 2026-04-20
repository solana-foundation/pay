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
    format_authorization, parse_www_authenticate, PaymentChallenge, PaymentCredential,
    SessionAction, SessionRequest,
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
    use solana_mpp::client::multi_delegate::{
        build_init_multi_delegate_tx, build_update_delegation_tx,
    };
    use solana_mpp::program::multi_delegator::MULTI_DELEGATOR_PROGRAM_ID;
    use solana_mpp::protocol::solana::{default_rpc_url, programs};
    use solana_mpp::solana_keychain::MemorySigner;
    use solana_pubkey::Pubkey;
    use std::str::FromStr;

    let network = network_override
        .map(str::to_string)
        .unwrap_or_else(|| request.network.clone().unwrap_or_else(|| "mainnet".to_string()));

    // Load the user's wallet keypair
    let (signer, ephemeral_notice) =
        crate::signer::load_signer_for_network_with_reason(
            &network,
            store,
            account_override,
            "authorize payment",
        )?;
    let user_pubkey = signer.pubkey();

    // Resolve RPC endpoint
    let rpc_url = std::env::var("PAY_RPC_URL")
        .unwrap_or_else(|_| default_rpc_url(&network).to_string());

    // Operator pubkey (delegatee in every FixedDelegation)
    let operator_pk = Pubkey::from_str(&request.operator)
        .map_err(|_| Error::Mpp(format!("invalid operator pubkey: {}", request.operator)))?;

    // Mint and token program (currency field carries the resolved mint address)
    let mint_pk = Pubkey::from_str(&request.currency)
        .map_err(|_| Error::Mpp(format!("invalid mint address in challenge: {}", request.currency)))?;
    let token_program_pk =
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();

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
        if let Err(e) =
            rt.block_on(crate::client::sandbox::fund_via_surfpool(&rpc, &pubkey))
        {
            tracing::warn!(error = %e, "Surfpool auto-fund failed — USDC balance may be 0");
        }
    }

    // Step 2: get a recent blockhash (sync RpcClient, fine in a sync context)
    let recent_blockhash = {
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
