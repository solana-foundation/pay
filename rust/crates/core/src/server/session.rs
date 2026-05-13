//! Server-side session intent — channel lifecycle and voucher verification.
//!
//! Wraps [`solana_mpp::server::session::SessionServer`] with an in-memory
//! channel store and provides challenge issuance + action dispatch that fits
//! the pay-core middleware pattern.
//!
//! # Pull-mode session flow
//!
//! ```text
//! Client sends `open` with a payer-signed payment-channel transaction
//!   │
//!   ▼
//! Server validates the transaction against the challenge and co-signs it
//!   │
//!   ▼
//! Server submits the transaction, then records channel state
//! ```
//!
//! Multi-delegator accounts are **long-lived**: most returning clients take the
//! "already sufficient" path with zero on-chain overhead.

use solana_mpp::program::multi_delegator::{
    MultiDelegateOnChainState, MultiDelegateSetupAction, assess_multi_delegate_setup,
};
use solana_mpp::server::session::{FinalizeParams, SessionConfig, SessionServer};
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::store::{ChannelState, MemoryChannelStore};
use solana_mpp::{
    Base64UrlJson, CommitReceipt, OpenPayload, PaymentChallenge, SessionAction, SessionMode,
    parse_authorization,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::{Error, Result};

const INTENT: &str = "session";
const METHOD: &str = "solana";
const DEFAULT_REALM: &str = "MPP Session";
const FIXED_DELEGATION_CAP_OFFSET: usize = 107;
const FIXED_DELEGATION_CAP_LEN: usize = 8;

// ── Multi-delegate chain interface ─────────────────────────────────────────

/// Async interface for querying and updating multi-delegator on-chain state.
///
/// Abstracting this out makes the session logic unit-testable without a live
/// Solana cluster.  In production, wire up a concrete implementation backed
/// by `solana-rpc-client`.
pub trait MultiDelegateChain: Send + Sync {
    /// Fetch the current `MultiDelegate` + `FixedDelegation` state for
    /// `owner` (client's wallet pubkey, base58).
    fn fetch_state<'a>(
        &'a self,
        owner: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<MultiDelegateOnChainState>> + Send + 'a>>;

    /// Submit a base64-encoded Solana transaction and return its signature.
    ///
    /// The implementation should block until the transaction is confirmed
    /// (or return an error if it fails / times out).
    fn submit_tx<'a>(
        &'a self,
        tx_base64: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
}

// ── Pull-mode setup outcome ────────────────────────────────────────────────

/// Outcome of the multi-delegator pre-flight check for a pull-mode `open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullSetupOutcome {
    /// Existing delegation already covered the cap — no tx was submitted.
    AlreadySufficient,
    /// `initDelegationTx` was submitted successfully.
    InitSubmitted { signature: String },
    /// `updateDelegationTx` was submitted successfully.
    UpdateSubmitted { signature: String },
}

/// Run the multi-delegator pre-flight check for a pull-mode `open` action.
///
/// 1. Fetches on-chain state via `chain.fetch_state(owner)`.
/// 2. Calls [`assess_multi_delegate_setup`] to decide what (if anything)
///    needs to happen.
/// 3. Submits the appropriate transaction or returns an error if a required
///    payload is missing.
///
/// This is a **free function** (not a method on `SessionMpp`) so it can be
/// called directly in unit tests with a mock chain.
pub async fn handle_pull_setup(
    payload: &OpenPayload,
    required_cap: u64,
    chain: &dyn MultiDelegateChain,
) -> Result<PullSetupOutcome> {
    let owner = payload
        .owner
        .as_deref()
        .ok_or_else(|| Error::Mpp("pull open missing owner".to_string()))?;

    tracing::debug!(
        owner,
        required_cap,
        "pull open: fetching multi-delegate on-chain state"
    );

    let on_chain = chain.fetch_state(owner).await.map_err(|e| {
        tracing::error!(owner, %e, "failed to fetch multi-delegate state");
        e
    })?;

    tracing::debug!(
        multi_delegate_exists = on_chain.multi_delegate_exists,
        existing_cap = ?on_chain.existing_delegation_cap,
        "multi-delegate on-chain state retrieved"
    );

    let action = assess_multi_delegate_setup(
        &on_chain,
        required_cap,
        payload.init_multi_delegate_tx.is_some(),
        payload.update_delegation_tx.is_some(),
    );

    tracing::info!(
        owner,
        required_cap,
        action = %action,
        "multi-delegate setup assessment"
    );

    match action {
        MultiDelegateSetupAction::AlreadySufficient => {
            tracing::debug!(owner, "multi-delegate already sufficient — skipping tx");
            Ok(PullSetupOutcome::AlreadySufficient)
        }

        MultiDelegateSetupAction::SubmitInit => {
            // SAFETY: `has_init_tx` was true → field is Some.
            let tx = payload.init_multi_delegate_tx.as_deref().unwrap();
            tracing::info!(owner, "submitting initDelegationTx");
            let sig = chain.submit_tx(tx).await.map_err(|e| {
                tracing::error!(owner, %e, "initDelegationTx failed");
                e
            })?;
            tracing::info!(owner, signature = %sig, "initDelegationTx confirmed");
            Ok(PullSetupOutcome::InitSubmitted { signature: sig })
        }

        MultiDelegateSetupAction::SubmitUpdate => {
            // SAFETY: `has_update_tx` was true → field is Some.
            let tx = payload.update_delegation_tx.as_deref().unwrap();
            tracing::info!(owner, "submitting UpdateDelegation tx");
            let sig = chain.submit_tx(tx).await.map_err(|e| {
                tracing::error!(owner, %e, "UpdateDelegation tx failed");
                e
            })?;
            tracing::info!(owner, signature = %sig, "UpdateDelegation tx confirmed");
            Ok(PullSetupOutcome::UpdateSubmitted { signature: sig })
        }

        MultiDelegateSetupAction::MissingPayload(reason) => {
            let reason = normalize_pull_setup_reason(&reason.to_string());
            tracing::warn!(owner, %reason, "pull open rejected: missing tx payload");
            Err(Error::Mpp(format!(
                "pull open requires on-chain setup: {reason}"
            )))
        }
    }
}

fn normalize_pull_setup_reason(reason: &str) -> String {
    reason.replace("initMultiDelegateTx", "initDelegationTx")
}

fn parse_fixed_delegation_cap(data: &[u8]) -> Option<u64> {
    let bytes = data
        .get(FIXED_DELEGATION_CAP_OFFSET..FIXED_DELEGATION_CAP_OFFSET + FIXED_DELEGATION_CAP_LEN)?;
    let bytes: [u8; FIXED_DELEGATION_CAP_LEN] = bytes.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

// ── RPC-backed multi-delegate chain ───────────────────────────────────────────

/// [`MultiDelegateChain`] implementation backed by a live Solana RPC endpoint.
///
/// Fetches `MultiDelegate` + `FixedDelegation` account state and submits
/// pre-signed base64 transactions.  Blocking RPC calls run on tokio's
/// blocking-thread pool so they don't starve the async executor.
pub struct RpcMultiDelegateChain {
    /// Solana RPC endpoint URL.
    pub rpc_url: String,
    /// Multi-delegator program address.
    pub program_id: solana_pubkey::Pubkey,
    /// SPL token mint (e.g. USDC).
    pub mint: solana_pubkey::Pubkey,
    /// Operator public key — the `delegatee` in every `FixedDelegation`.
    pub operator: solana_pubkey::Pubkey,
    /// Nonce used to derive the `FixedDelegation` PDA.
    pub delegation_nonce: u64,
}

impl MultiDelegateChain for RpcMultiDelegateChain {
    fn fetch_state<'a>(
        &'a self,
        owner: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<MultiDelegateOnChainState>> + Send + 'a>> {
        use solana_mpp::program::multi_delegator::{
            find_fixed_delegation_pda, find_multi_delegate_pda,
        };

        let owner_str = owner.to_string();
        let rpc_url = self.rpc_url.clone();
        let program_id = self.program_id;
        let mint = self.mint;
        let operator = self.operator;
        let nonce = self.delegation_nonce;

        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> Result<MultiDelegateOnChainState> {
                use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
                use std::str::FromStr;

                let owner_pk = solana_pubkey::Pubkey::from_str(&owner_str)
                    .map_err(|e| Error::Mpp(format!("invalid owner pubkey: {e}")))?;

                let (multi_delegate_pda, _) =
                    find_multi_delegate_pda(&owner_pk, &mint, &program_id);
                let (delegation_pda, _) = find_fixed_delegation_pda(
                    &multi_delegate_pda,
                    &owner_pk,
                    &operator,
                    nonce,
                    &program_id,
                );

                let rpc = RpcClient::new(rpc_url);
                let accounts = rpc
                    .get_multiple_accounts(&[multi_delegate_pda, delegation_pda])
                    .map_err(|e| {
                        Error::Mpp(format!("RPC error fetching delegation accounts: {e}"))
                    })?;

                let multi_delegate_exists = accounts[0].is_some();

                // FixedDelegation account layout:
                //   [0..107]   header
                //   [107..115] delegated amount: u64
                // RPC account data is untrusted here, so malformed or short
                // accounts must not panic the gateway.
                let existing_delegation_cap = accounts[1]
                    .as_ref()
                    .and_then(|acct| parse_fixed_delegation_cap(&acct.data));

                tracing::info!(
                    %owner_str,
                    %multi_delegate_exists,
                    ?existing_delegation_cap,
                    "RPC multi-delegate state fetched"
                );

                Ok(MultiDelegateOnChainState {
                    multi_delegate_exists,
                    existing_delegation_cap,
                })
            })
            .await
            .map_err(|e| Error::Mpp(format!("spawn_blocking join error: {e}")))?
        })
    }

    fn submit_tx<'a>(
        &'a self,
        tx_base64: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        let rpc_url = self.rpc_url.clone();
        let tx_b64 = tx_base64.to_string();

        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> Result<String> {
                use base64::Engine;
                use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
                use solana_transaction::Transaction;

                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&tx_b64)
                    .map_err(|e| Error::Mpp(format!("invalid base64 tx: {e}")))?;
                let tx: Transaction = bincode::deserialize(&bytes)
                    .map_err(|e| Error::Mpp(format!("tx deserialization failed: {e}")))?;

                let rpc = RpcClient::new(rpc_url);
                let sig = rpc
                    .send_and_confirm_transaction(&tx)
                    .map_err(|e| Error::Mpp(format!("tx submission failed: {e}")))?;

                tracing::info!(signature = %sig, "multi-delegate tx confirmed on-chain");
                Ok(sig.to_string())
            })
            .await
            .map_err(|e| Error::Mpp(format!("spawn_blocking join error: {e}")))?
        })
    }
}

// ── Session outcome ────────────────────────────────────────────────────────

/// The result of processing a session action.
pub enum SessionOutcome {
    /// `open` or `topup` — channel state after the action.
    Active(ChannelState),
    /// `voucher` accepted — the new settled cumulative (base units).
    Voucher(u64),
    /// `commit` accepted — receipt for the metered delivery.
    Commit(CommitReceipt),
    /// `close` accepted — `FinalizeParams` carries what's needed to submit the
    /// on-chain finalize + distribute transactions.
    Closed(FinalizeParams),
}

// ── Session manager ────────────────────────────────────────────────────────

/// Server-side session manager.
///
/// Holds a [`SessionServer`] backed by an in-memory channel store.  For
/// production, swap `MemoryChannelStore` with a persistent backend.
///
/// Payment-channel push sessions submit a client-signed transaction that the
/// server validates and co-signs. Pull-mode delegation setup remains available
/// for compatibility, but it no longer opens a synthetic channel.
pub struct SessionMpp {
    server: SessionServer<MemoryChannelStore>,
    session_config: SessionConfig,
    secret_key: String,
    realm: String,
    rpc_url: Option<String>,
    payment_channel_signer: Option<Arc<dyn SolanaSigner>>,
    /// Interface to on-chain multi-delegate state (optional; pull-mode setup
    /// is skipped when absent).
    multi_delegate_chain: Option<Box<dyn MultiDelegateChain>>,
}

impl SessionMpp {
    /// Create from a [`SessionConfig`] and an HMAC secret key.
    pub fn new(config: SessionConfig, secret_key: impl Into<String>) -> Self {
        let session_config = config.clone();
        Self {
            rpc_url: config.rpc_url.clone(),
            server: SessionServer::new(config, MemoryChannelStore::new()),
            session_config,
            secret_key: secret_key.into(),
            realm: DEFAULT_REALM.to_string(),
            payment_channel_signer: None,
            multi_delegate_chain: None,
        }
    }

    pub fn with_realm(mut self, realm: impl Into<String>) -> Self {
        self.realm = realm.into();
        self
    }

    /// Wire up on-chain multi-delegate state resolution for pull-mode sessions.
    ///
    /// When set, every pull-mode `open` will:
    /// 1. Fetch the client's `MultiDelegate` + `FixedDelegation` state.
    /// 2. Submit a setup tx if the delegation is missing or insufficient.
    pub fn with_multi_delegate_chain(mut self, chain: Box<dyn MultiDelegateChain>) -> Self {
        self.multi_delegate_chain = Some(chain);
        self
    }

    /// Configure the operator signer used to co-sign client-provided
    /// payment-channel open transactions and to submit close settlement txs.
    pub fn with_payment_channel_signer(mut self, signer: Arc<dyn SolanaSigner>) -> Self {
        self.payment_channel_signer = Some(signer);
        self
    }

    /// Build a [`PaymentChallenge`] for a new session with the given cap.
    pub fn challenge(&self, cap: u64) -> Result<PaymentChallenge> {
        let mut request = self.server.build_challenge_request(cap);
        request.recent_blockhash = self.fetch_recent_blockhash();
        let encoded = Base64UrlJson::from_typed(&request)
            .map_err(|e| Error::Mpp(format!("Failed to encode session request: {e}")))?;
        Ok(PaymentChallenge::with_secret_key(
            &self.secret_key,
            &self.realm,
            METHOD,
            INTENT,
            encoded,
        ))
    }

    /// Format a session challenge as a `WWW-Authenticate` header value.
    pub fn challenge_header(&self, cap: u64) -> Result<String> {
        self.challenge(cap)?
            .to_header()
            .map_err(|e| Error::Mpp(format!("Failed to format session challenge: {e}")))
    }

    /// Process an `Authorization` header containing a [`SessionAction`].
    ///
    /// For payment-channel `open` actions carrying `transaction`, the server
    /// validates the embedded open instruction, co-signs, submits, then stores
    /// the confirmed channel.
    pub async fn process(&self, auth_header: &str) -> Result<SessionOutcome> {
        let credential = parse_authorization(auth_header)
            .map_err(|e| Error::Mpp(format!("Invalid authorization header: {e}")))?;

        if credential.challenge.intent.as_str() != INTENT {
            return Err(Error::Mpp(format!(
                "Expected '{}' intent, got '{}'",
                INTENT, credential.challenge.intent
            )));
        }

        let action: SessionAction = serde_json::from_value(credential.payload)
            .map_err(|e| Error::Mpp(format!("Unrecognized session action payload: {e}")))?;

        match &action {
            SessionAction::Open(p) => {
                if p.mode == SessionMode::Pull {
                    self.run_pull_setup(p).await?;
                }

                let mut submitted_open = None;
                let open_payload;
                let payload_for_open = if p.mode == SessionMode::Push {
                    if let Some(signature) = self.submit_payment_channel_open(p).await? {
                        open_payload = {
                            let mut payload = p.clone();
                            payload.signature = signature.clone();
                            payload
                        };
                        submitted_open = Some(signature);
                        &open_payload
                    } else {
                        p
                    }
                } else {
                    p
                };

                let state = self
                    .server
                    .process_open(payload_for_open)
                    .await
                    .map_err(|e| Error::Mpp(format!("Session open failed: {e}")))?;

                if let Some(signature) = submitted_open {
                    tracing::info!(%signature, "payment-channel open transaction confirmed");
                }

                Ok(SessionOutcome::Active(state))
            }

            SessionAction::Voucher(p) => {
                let cumulative = self
                    .server
                    .verify_voucher(p)
                    .await
                    .map_err(|e| Error::PaymentRejected(e.to_string()))?;
                Ok(SessionOutcome::Voucher(cumulative))
            }

            SessionAction::Commit(p) => {
                let receipt = self
                    .server
                    .process_commit(p)
                    .await
                    .map_err(|e| Error::PaymentRejected(e.to_string()))?;
                Ok(SessionOutcome::Commit(receipt))
            }

            SessionAction::TopUp(p) => {
                let state = self
                    .server
                    .process_topup(p)
                    .await
                    .map_err(|e| Error::Mpp(format!("TopUp failed: {e}")))?;
                Ok(SessionOutcome::Active(state))
            }

            SessionAction::Close(p) => {
                let params = self
                    .server
                    .process_close(p)
                    .await
                    .map_err(|e| Error::Mpp(format!("Session close failed: {e}")))?;
                if let Some(signature) = self.submit_payment_channel_settlement(&params).await? {
                    self.server
                        .mark_finalized(&params.channel_id.to_string())
                        .await
                        .map_err(|e| {
                            Error::Mpp(format!("Failed to mark session finalized: {e}"))
                        })?;
                    tracing::info!(%signature, channel = %params.channel_id, "payment-channel settlement confirmed");
                }
                Ok(SessionOutcome::Closed(params))
            }
        }
    }

    /// Retrieve finalize parameters for an open channel.
    pub async fn finalize_params(&self, channel_id: &str) -> Result<FinalizeParams> {
        self.server
            .finalize_params(channel_id)
            .await
            .map_err(|e| Error::Mpp(format!("Failed to get finalize params: {e}")))
    }

    /// Reserve a metered delivery so a client can later acknowledge it with a
    /// signed `commit` voucher.
    pub async fn begin_delivery(
        &self,
        request: solana_mpp::server::session::DeliveryRequest,
    ) -> Result<solana_mpp::MeteringDirective> {
        self.server
            .begin_delivery(request)
            .await
            .map_err(|e| Error::Mpp(format!("Failed to reserve session delivery: {e}")))
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Run the multi-delegator pre-flight for a pull-mode open.
    ///
    /// Skips silently if no chain is configured (useful for tests or push-only
    /// deployments).
    async fn run_pull_setup(&self, payload: &OpenPayload) -> Result<()> {
        let chain = match &self.multi_delegate_chain {
            Some(c) => c.as_ref(),
            None => {
                tracing::info!("no multi-delegate chain configured — skipping pull setup");
                return Ok(());
            }
        };

        let required_cap = payload
            .deposit_amount()
            .map_err(|e| Error::Mpp(format!("pull open: {e}")))?;

        handle_pull_setup(payload, required_cap, chain).await?;
        Ok(())
    }

    async fn submit_payment_channel_open(&self, payload: &OpenPayload) -> Result<Option<String>> {
        let Some(transaction) = payload.transaction.as_deref() else {
            return Ok(None);
        };
        let signer = self.payment_channel_signer.as_ref().ok_or_else(|| {
            Error::Mpp("payment-channel open transaction requires an operator signer".to_string())
        })?;
        let rpc_url = self.rpc_url.clone().ok_or_else(|| {
            Error::Mpp("payment-channel open transaction requires an RPC URL".to_string())
        })?;

        let mut tx = decode_base64_transaction(transaction)?;
        let expected = self.expected_payment_channel_open_instruction(payload)?;
        validate_payment_channel_open_transaction(&tx, &expected, &signer.pubkey())?;

        sign_and_submit_transaction(Arc::clone(signer), rpc_url, &mut tx, "payment-channel open")
            .await
            .map(Some)
    }

    async fn submit_payment_channel_settlement(
        &self,
        params: &FinalizeParams,
    ) -> Result<Option<String>> {
        let Some(signer) = self.payment_channel_signer.as_ref() else {
            return Ok(None);
        };
        let rpc_url = self.rpc_url.clone().ok_or_else(|| {
            Error::Mpp("payment-channel settlement requires an RPC URL".to_string())
        })?;
        let payer = params
            .payer
            .ok_or_else(|| Error::Mpp("payment-channel settlement missing payer".to_string()))?;
        let mint = params
            .mint
            .ok_or_else(|| Error::Mpp("payment-channel settlement missing mint".to_string()))?;
        let authorized_signer = params.authorized_signer.ok_or_else(|| {
            Error::Mpp("payment-channel settlement missing authorized signer".to_string())
        })?;
        let token_program = spl_token_program();

        let signature = match params.voucher_signature.as_deref() {
            Some(signature) => Some(decode_voucher_signature(signature)?),
            None if params.settled == 0 => None,
            None => {
                return Err(Error::Mpp(
                    "payment-channel settlement missing highest voucher signature".to_string(),
                ));
            }
        };
        let expires_at = params.voucher_expires_at.unwrap_or(0);

        let mut instructions =
            solana_mpp::program::payment_channels::build_settle_and_finalize_instructions(
                &params.recipient,
                &params.channel_id,
                &authorized_signer,
                signature.as_ref(),
                params.settled,
                expires_at,
                &params.program_id,
            )
            .map_err(|e| Error::Mpp(format!("failed to build settlement instruction: {e}")))?;
        let recipients = params
            .splits
            .iter()
            .map(
                |split| solana_mpp::program::payment_channels::Distribution {
                    recipient: split.recipient,
                    bps: split.bps,
                },
            )
            .collect::<Vec<_>>();
        instructions.push(
            solana_mpp::program::payment_channels::build_distribute_instruction(
                &params.channel_id,
                &payer,
                &params.recipient,
                &solana_mpp::program::payment_channels::treasury_owner(),
                &mint,
                &recipients,
                &token_program,
                &params.program_id,
            ),
        );

        let blockhash = fetch_latest_blockhash(&rpc_url)?;
        let fee_payer = signer.pubkey();
        let message = solana_message::Message::new_with_blockhash(
            &instructions,
            Some(&fee_payer),
            &blockhash,
        );
        let mut tx = solana_transaction::Transaction::new_unsigned(message);
        sign_and_submit_transaction(
            Arc::clone(signer),
            rpc_url,
            &mut tx,
            "payment-channel settlement",
        )
        .await
        .map(Some)
    }

    fn expected_payment_channel_open_instruction(
        &self,
        payload: &OpenPayload,
    ) -> Result<solana_instruction::Instruction> {
        let params = self.payment_channel_open_params(payload)?;
        Ok(solana_mpp::program::payment_channels::build_open_instruction(&params))
    }

    fn payment_channel_open_params(
        &self,
        payload: &OpenPayload,
    ) -> Result<solana_mpp::program::payment_channels::OpenChannelParams> {
        let payer = parse_pubkey_field(payload.payer.as_deref(), "payer")?;
        let payee = parse_pubkey_field(payload.payee.as_deref(), "payee")?;
        let mint = parse_pubkey_field(payload.mint.as_deref(), "mint")?;
        let authorized_signer = parse_pubkey_value(&payload.authorized_signer, "authorizedSigner")?;
        let salt = payload
            .salt
            .ok_or_else(|| Error::Mpp("payment-channel open missing salt".to_string()))?;
        let grace_period = payload
            .grace_period
            .ok_or_else(|| Error::Mpp("payment-channel open missing gracePeriod".to_string()))?;
        let deposit = payload
            .deposit_amount()
            .map_err(|e| Error::Mpp(format!("payment-channel open: {e}")))?;
        let token_program = spl_token_program();
        let program_id = self
            .session_config
            .program_id
            .unwrap_or_else(solana_mpp::program::payment_channels::default_program_id);

        if let Ok(expected_payee) = parse_pubkey_value(&self.session_config.recipient, "recipient")
            && payee != expected_payee
        {
            return Err(Error::Mpp(
                "payment-channel open payee does not match challenge recipient".to_string(),
            ));
        }
        if let Ok(expected_mint) = parse_pubkey_value(&self.session_config.currency, "currency")
            && mint != expected_mint
        {
            return Err(Error::Mpp(
                "payment-channel open mint does not match challenge currency".to_string(),
            ));
        }

        let recipients = self
            .session_config
            .splits
            .iter()
            .map(
                |split| solana_mpp::program::payment_channels::Distribution {
                    recipient: split.recipient,
                    bps: split.bps,
                },
            )
            .collect();
        let params = solana_mpp::program::payment_channels::OpenChannelParams {
            payer,
            payee,
            mint,
            authorized_signer,
            salt,
            deposit,
            grace_period,
            recipients,
            token_program,
            program_id,
        };

        let expected_channel =
            solana_mpp::program::payment_channels::derive_channel_addresses(&params).channel;
        let channel = parse_pubkey_field(payload.channel_id.as_deref(), "channelId")?;
        if channel != expected_channel {
            return Err(Error::Mpp(
                "payment-channel open channelId does not match derived channel PDA".to_string(),
            ));
        }

        Ok(params)
    }

    /// Best-effort blockhash prefetch for session challenges.
    ///
    /// The challenge remains valid without this field, so RPC failures are
    /// logged and ignored instead of failing challenge generation.
    fn fetch_recent_blockhash(&self) -> Option<String> {
        use solana_mpp::solana_rpc_client::rpc_client::RpcClient;

        let rpc_url = self.rpc_url.as_ref()?;
        match RpcClient::new(rpc_url.clone()).get_latest_blockhash() {
            Ok(blockhash) => Some(blockhash.to_string()),
            Err(error) => {
                tracing::debug!(rpc_url, %error, "failed to prefetch session recent blockhash");
                None
            }
        }
    }
}

fn spl_token_program() -> solana_pubkey::Pubkey {
    use std::str::FromStr;
    solana_pubkey::Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
        .expect("valid SPL token program id")
}

fn parse_pubkey_field(value: Option<&str>, field: &str) -> Result<solana_pubkey::Pubkey> {
    let value = value.ok_or_else(|| Error::Mpp(format!("payment-channel open missing {field}")))?;
    parse_pubkey_value(value, field)
}

fn parse_pubkey_value(value: &str, field: &str) -> Result<solana_pubkey::Pubkey> {
    use std::str::FromStr;
    solana_pubkey::Pubkey::from_str(value)
        .map_err(|e| Error::Mpp(format!("invalid payment-channel {field}: {e}")))
}

fn decode_base64_transaction(tx_base64: &str) -> Result<solana_transaction::Transaction> {
    use base64::Engine;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(tx_base64)
        .map_err(|e| Error::Mpp(format!("invalid base64 transaction: {e}")))?;
    bincode::deserialize(&bytes)
        .map_err(|e| Error::Mpp(format!("transaction deserialization failed: {e}")))
}

fn decode_voucher_signature(signature: &str) -> Result<[u8; 64]> {
    let bytes = bs58::decode(signature)
        .into_vec()
        .map_err(|e| Error::Mpp(format!("invalid voucher signature encoding: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| Error::Mpp("voucher signature is not 64 bytes".to_string()))
}

fn transaction_contains_instruction(
    tx: &solana_transaction::Transaction,
    expected: &solana_instruction::Instruction,
) -> bool {
    tx.message.instructions.iter().any(|compiled| {
        let Some(program_id) = tx
            .message
            .account_keys
            .get(compiled.program_id_index as usize)
        else {
            return false;
        };
        if program_id != &expected.program_id || compiled.data != expected.data {
            return false;
        }

        let accounts = compiled
            .accounts
            .iter()
            .filter_map(|index| tx.message.account_keys.get(*index as usize).copied())
            .collect::<Vec<_>>();
        let expected_accounts = expected
            .accounts
            .iter()
            .map(|account| account.pubkey)
            .collect::<Vec<_>>();
        accounts == expected_accounts
    })
}

fn validate_payment_channel_open_transaction(
    tx: &solana_transaction::Transaction,
    expected: &solana_instruction::Instruction,
    fee_payer: &solana_pubkey::Pubkey,
) -> Result<()> {
    if tx.message.account_keys.first() != Some(fee_payer) {
        return Err(Error::Mpp(
            "payment-channel open transaction fee payer does not match operator".to_string(),
        ));
    }

    if tx.message.instructions.len() != 1 {
        return Err(Error::Mpp(
            "payment-channel open transaction must contain exactly one instruction".to_string(),
        ));
    }

    if !transaction_contains_instruction(tx, expected) {
        return Err(Error::Mpp(
            "payment-channel open transaction does not match the session challenge".to_string(),
        ));
    }

    Ok(())
}

fn fetch_latest_blockhash(rpc_url: &str) -> Result<solana_hash::Hash> {
    use solana_mpp::solana_rpc_client::rpc_client::RpcClient;

    RpcClient::new(rpc_url.to_string())
        .get_latest_blockhash()
        .map_err(|e| Error::Mpp(format!("failed to fetch latest blockhash: {e}")))
}

async fn sign_and_submit_transaction(
    signer: Arc<dyn SolanaSigner>,
    rpc_url: String,
    tx: &mut solana_transaction::Transaction,
    context: &'static str,
) -> Result<String> {
    signer
        .sign_transaction(tx)
        .await
        .map_err(|e| Error::Mpp(format!("failed to sign {context} transaction: {e}")))?;
    let tx = tx.clone();

    tokio::task::spawn_blocking(move || {
        use solana_mpp::solana_rpc_client::rpc_client::RpcClient;

        RpcClient::new(rpc_url)
            .send_and_confirm_transaction(&tx)
            .map(|signature| signature.to_string())
            .map_err(|e| Error::Mpp(format!("{context} transaction submission failed: {e}")))
    })
    .await
    .map_err(|e| Error::Mpp(format!("spawn_blocking join error: {e}")))?
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::session::SessionHandle;
    use solana_mpp::program::multi_delegator::MultiDelegateOnChainState;
    use solana_mpp::solana_keychain::{SolanaSigner, memory::MemorySigner};
    use solana_mpp::{PaymentCredential, format_authorization};
    use std::sync::{Arc, Mutex};

    // ── Mock MultiDelegateChain ───────────────────────────────────────────────

    struct MockChain {
        state: MultiDelegateOnChainState,
        submitted: Arc<Mutex<Vec<String>>>,
        submit_error: Option<String>,
    }

    impl MockChain {
        fn with_state(state: MultiDelegateOnChainState) -> Self {
            Self {
                state,
                submitted: Arc::new(Mutex::new(vec![])),
                submit_error: None,
            }
        }

        fn with_submit_error(mut self, msg: &str) -> Self {
            self.submit_error = Some(msg.to_string());
            self
        }

        fn submitted_txs(&self) -> Vec<String> {
            self.submitted.lock().unwrap().clone()
        }
    }

    impl MultiDelegateChain for MockChain {
        fn fetch_state<'a>(
            &'a self,
            _owner: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<MultiDelegateOnChainState>> + Send + 'a>> {
            let state = self.state.clone();
            Box::pin(async move { Ok(state) })
        }

        fn submit_tx<'a>(
            &'a self,
            tx_base64: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            if let Some(ref err) = self.submit_error {
                let e = err.clone();
                return Box::pin(async move { Err(Error::Mpp(e)) });
            }
            let submitted = Arc::clone(&self.submitted);
            let tx = tx_base64.to_string();
            Box::pin(async move {
                submitted.lock().unwrap().push(tx);
                Ok("mock_sig_abc123".to_string())
            })
        }
    }

    // ── Payload helpers ───────────────────────────────────────────────────────

    fn no_tx_payload(required_cap: u64) -> OpenPayload {
        OpenPayload::pull(
            "tokacct111".to_string(),
            required_cap.to_string(),
            "walletABC".to_string(),
            "signer1".to_string(),
            "sig1".to_string(),
        )
    }

    fn init_tx_payload(required_cap: u64) -> OpenPayload {
        no_tx_payload(required_cap).with_init_tx("init_tx_base64".to_string())
    }

    fn update_tx_payload(required_cap: u64) -> OpenPayload {
        no_tx_payload(required_cap).with_update_tx("update_tx_base64".to_string())
    }

    fn both_tx_payload(required_cap: u64) -> OpenPayload {
        no_tx_payload(required_cap)
            .with_init_tx("init_tx_base64".to_string())
            .with_update_tx("update_tx_base64".to_string())
    }

    fn chain_no_pda() -> MockChain {
        MockChain::with_state(MultiDelegateOnChainState {
            multi_delegate_exists: false,
            existing_delegation_cap: None,
        })
    }

    fn chain_pda_no_delegation() -> MockChain {
        MockChain::with_state(MultiDelegateOnChainState {
            multi_delegate_exists: true,
            existing_delegation_cap: None,
        })
    }

    fn chain_insufficient(cap: u64) -> MockChain {
        MockChain::with_state(MultiDelegateOnChainState {
            multi_delegate_exists: true,
            existing_delegation_cap: Some(cap),
        })
    }

    fn chain_sufficient(cap: u64) -> MockChain {
        MockChain::with_state(MultiDelegateOnChainState {
            multi_delegate_exists: true,
            existing_delegation_cap: Some(cap),
        })
    }

    const CAP: u64 = 1_000_000;

    fn test_session_config() -> SessionConfig {
        SessionConfig {
            operator: solana_pubkey::Pubkey::new_unique().to_string(),
            recipient: solana_pubkey::Pubkey::new_unique().to_string(),
            max_cap: 5 * CAP,
            currency: solana_pubkey::Pubkey::new_unique().to_string(),
            network: "localnet".to_string(),
            modes: vec![SessionMode::Push, SessionMode::Pull],
            ..SessionConfig::default()
        }
    }

    fn test_session_mpp() -> SessionMpp {
        SessionMpp::new(test_session_config(), "test-secret")
    }

    fn test_session_signer() -> Box<dyn SolanaSigner> {
        use ed25519_dalek::SigningKey;

        let sk = SigningKey::generate(&mut rand::thread_rng());
        let vk = sk.verifying_key();
        let mut kp = [0u8; 64];
        kp[..32].copy_from_slice(sk.as_bytes());
        kp[32..].copy_from_slice(vk.as_bytes());
        Box::new(MemorySigner::from_bytes(&kp).unwrap())
    }

    // ── handle_pull_setup: AlreadySufficient path ─────────────────────────────

    #[tokio::test]
    async fn already_sufficient_returns_ok_no_tx_submitted() {
        let chain = chain_sufficient(5 * CAP);
        let outcome = handle_pull_setup(&no_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap();
        assert_eq!(outcome, PullSetupOutcome::AlreadySufficient);
        assert!(
            chain.submitted_txs().is_empty(),
            "no tx should be submitted"
        );
    }

    #[tokio::test]
    async fn exact_cap_returns_already_sufficient() {
        let chain = chain_sufficient(CAP);
        let outcome = handle_pull_setup(&no_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap();
        assert_eq!(outcome, PullSetupOutcome::AlreadySufficient);
    }

    #[tokio::test]
    async fn already_sufficient_ignores_provided_update_tx() {
        let chain = chain_sufficient(5 * CAP);
        let outcome = handle_pull_setup(&update_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap();
        assert_eq!(outcome, PullSetupOutcome::AlreadySufficient);
        assert!(
            chain.submitted_txs().is_empty(),
            "update tx must not be submitted when cap sufficient"
        );
    }

    // ── handle_pull_setup: SubmitInit path ────────────────────────────────────

    #[tokio::test]
    async fn no_multi_delegate_with_init_tx_submits_init() {
        let chain = chain_no_pda();
        let outcome = handle_pull_setup(&init_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PullSetupOutcome::InitSubmitted {
                signature: "mock_sig_abc123".to_string()
            }
        );
        assert_eq!(chain.submitted_txs(), vec!["init_tx_base64"]);
    }

    #[tokio::test]
    async fn no_multi_delegate_with_both_txs_submits_only_init() {
        let chain = chain_no_pda();
        let outcome = handle_pull_setup(&both_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PullSetupOutcome::InitSubmitted {
                signature: "mock_sig_abc123".to_string()
            }
        );
        // Only init_tx was submitted, not update_tx
        assert_eq!(chain.submitted_txs(), vec!["init_tx_base64"]);
    }

    // ── handle_pull_setup: SubmitUpdate path ──────────────────────────────────

    #[tokio::test]
    async fn pda_exists_no_delegation_with_update_tx_submits_update() {
        let chain = chain_pda_no_delegation();
        let outcome = handle_pull_setup(&update_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PullSetupOutcome::UpdateSubmitted {
                signature: "mock_sig_abc123".to_string()
            }
        );
        assert_eq!(chain.submitted_txs(), vec!["update_tx_base64"]);
    }

    #[tokio::test]
    async fn pda_exists_insufficient_cap_with_update_tx_submits_update() {
        let chain = chain_insufficient(CAP / 2);
        let outcome = handle_pull_setup(&update_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PullSetupOutcome::UpdateSubmitted {
                signature: "mock_sig_abc123".to_string()
            }
        );
        assert_eq!(chain.submitted_txs(), vec!["update_tx_base64"]);
    }

    #[tokio::test]
    async fn pda_exists_insufficient_with_both_txs_submits_only_update() {
        let chain = chain_insufficient(CAP / 2);
        let outcome = handle_pull_setup(&both_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PullSetupOutcome::UpdateSubmitted {
                signature: "mock_sig_abc123".to_string()
            }
        );
        // Only update_tx was submitted, not init_tx
        assert_eq!(chain.submitted_txs(), vec!["update_tx_base64"]);
    }

    // ── handle_pull_setup: MissingPayload errors ──────────────────────────────

    #[tokio::test]
    async fn no_multi_delegate_without_init_tx_returns_error() {
        let chain = chain_no_pda();
        let err = handle_pull_setup(&no_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("initDelegationTx"),
            "expected init tx mention, got: {msg}"
        );
        assert!(chain.submitted_txs().is_empty());
    }

    #[tokio::test]
    async fn pda_exists_no_delegation_without_update_tx_returns_error() {
        let chain = chain_pda_no_delegation();
        let err = handle_pull_setup(&no_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("updateDelegationTx"),
            "expected update tx mention, got: {msg}"
        );
        assert!(chain.submitted_txs().is_empty());
    }

    #[tokio::test]
    async fn no_multi_delegate_with_update_tx_only_returns_error() {
        // update_tx alone is not enough when MultiDelegate doesn't exist yet.
        let chain = chain_no_pda();
        let err = handle_pull_setup(&update_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("initDelegationTx"),
            "expected init tx mention, got: {msg}"
        );
        assert!(chain.submitted_txs().is_empty());
    }

    // ── handle_pull_setup: tx submission failures ─────────────────────────────

    #[tokio::test]
    async fn init_tx_submission_failure_propagates_error() {
        let chain = chain_no_pda().with_submit_error("RPC timeout");
        let err = handle_pull_setup(&init_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("RPC timeout"), "got: {err}");
    }

    #[tokio::test]
    async fn update_tx_submission_failure_propagates_error() {
        let chain = chain_pda_no_delegation().with_submit_error("network error");
        let err = handle_pull_setup(&update_tx_payload(CAP), CAP, &chain)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("network error"), "got: {err}");
    }

    // ── handle_pull_setup: missing owner field ────────────────────────────────

    #[tokio::test]
    async fn missing_owner_returns_error() {
        let chain = chain_no_pda();
        // Manually construct a pull payload without owner
        let payload = OpenPayload {
            mode: solana_mpp::SessionMode::Pull,
            channel_id: None,
            deposit: None,
            payer: None,
            payee: None,
            mint: None,
            salt: None,
            grace_period: None,
            transaction: None,
            token_account: Some("tok".to_string()),
            approved_amount: Some("1000000".to_string()),
            owner: None, // <-- missing
            init_multi_delegate_tx: Some("init_tx".to_string()),
            update_delegation_tx: None,
            authorized_signer: "signer".to_string(),
            signature: "sig".to_string(),
        };
        let err = handle_pull_setup(&payload, CAP, &chain).await.unwrap_err();
        assert!(err.to_string().contains("owner"), "got: {err}");
    }

    // ── PullSetupOutcome display ──────────────────────────────────────────────

    #[test]
    fn pull_setup_outcomes_are_distinguishable() {
        let already = PullSetupOutcome::AlreadySufficient;
        let init = PullSetupOutcome::InitSubmitted {
            signature: "sig1".to_string(),
        };
        let update = PullSetupOutcome::UpdateSubmitted {
            signature: "sig2".to_string(),
        };
        assert_ne!(already, init);
        assert_ne!(already, update);
        assert_ne!(init, update);
    }

    #[test]
    fn normalize_pull_setup_reason_renames_init_payload() {
        assert_eq!(
            normalize_pull_setup_reason("missing initMultiDelegateTx"),
            "missing initDelegationTx"
        );
    }

    #[test]
    fn parse_fixed_delegation_cap_reads_expected_offset() {
        let mut data = vec![0u8; FIXED_DELEGATION_CAP_OFFSET + FIXED_DELEGATION_CAP_LEN];
        data[FIXED_DELEGATION_CAP_OFFSET..FIXED_DELEGATION_CAP_OFFSET + FIXED_DELEGATION_CAP_LEN]
            .copy_from_slice(&CAP.to_le_bytes());
        assert_eq!(parse_fixed_delegation_cap(&data), Some(CAP));
    }

    #[test]
    fn parse_fixed_delegation_cap_rejects_short_data() {
        let data = vec![0u8; FIXED_DELEGATION_CAP_OFFSET + FIXED_DELEGATION_CAP_LEN - 1];
        assert_eq!(parse_fixed_delegation_cap(&data), None);
    }

    #[test]
    fn with_realm_updates_challenge_realm() {
        let session = test_session_mpp().with_realm("Custom Realm");
        let challenge = session.challenge(CAP).unwrap();
        assert_eq!(challenge.realm, "Custom Realm");
    }

    #[test]
    fn fetch_recent_blockhash_without_rpc_returns_none() {
        assert_eq!(test_session_mpp().fetch_recent_blockhash(), None);
    }

    #[tokio::test]
    async fn process_rejects_non_session_intent() {
        let session = test_session_mpp();
        let challenge = PaymentChallenge::with_secret_key(
            "test-secret",
            "test-realm",
            METHOD,
            "charge",
            Base64UrlJson::from_typed(&session.server.build_challenge_request(CAP)).unwrap(),
        );
        let handle = SessionHandle::new(
            solana_pubkey::Pubkey::new_unique(),
            test_session_signer(),
            challenge,
        );
        let auth_header = handle.open_header(CAP, "open_sig").await.unwrap();

        let err = session
            .process(&auth_header)
            .await
            .err()
            .expect("non-session intent should error");
        assert!(
            err.to_string().contains("Expected 'session' intent"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn process_rejects_invalid_authorization_header() {
        let session = test_session_mpp();
        let err = session
            .process("Bearer definitely-not-mpp")
            .await
            .err()
            .expect("invalid auth should error");
        assert!(
            err.to_string().contains("Invalid authorization header"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn process_rejects_unknown_session_action_payload() {
        let session = test_session_mpp();
        let challenge = session.challenge(CAP).unwrap();
        let credential = PaymentCredential::new(
            challenge.to_echo(),
            serde_json::json!({ "action": "mystery" }),
        );
        let auth_header = format_authorization(&credential).unwrap();

        let err = session
            .process(&auth_header)
            .await
            .err()
            .expect("unknown action should error");
        assert!(
            err.to_string()
                .contains("Unrecognized session action payload"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn process_supports_open_voucher_topup_and_close() {
        let session = test_session_mpp();
        let challenge = session.challenge(CAP).unwrap();
        let handle = SessionHandle::new(
            solana_pubkey::Pubkey::new_unique(),
            test_session_signer(),
            challenge,
        );
        let open_header = handle.open_header(CAP, "open_sig").await.unwrap();

        let SessionOutcome::Active(opened) = session.process(&open_header).await.unwrap() else {
            panic!("expected open to return active session");
        };
        assert_eq!(opened.deposit, CAP);

        let voucher_header = handle.voucher_header(75).await.unwrap();
        let SessionOutcome::Voucher(cumulative) = session.process(&voucher_header).await.unwrap()
        else {
            panic!("expected voucher outcome");
        };
        assert_eq!(cumulative, 75);

        let topup_header = handle.topup_header(CAP + 500, "topup_sig").await.unwrap();
        let SessionOutcome::Active(topped_up) = session.process(&topup_header).await.unwrap()
        else {
            panic!("expected topup outcome");
        };
        assert_eq!(topped_up.deposit, CAP + 500);

        let close_header = handle.close_header(Some(25)).await.unwrap();
        let SessionOutcome::Closed(params) = session.process(&close_header).await.unwrap() else {
            panic!("expected close outcome");
        };
        assert_eq!(params.settled, 100);
    }

    #[tokio::test]
    async fn process_supports_reserved_delivery_commit() {
        let session = test_session_mpp();
        let challenge = session.challenge(CAP).unwrap();
        let channel_id = solana_pubkey::Pubkey::new_unique();
        let active =
            solana_mpp::client::session::ActiveSession::new(channel_id, test_session_signer());

        let open_action = active.open_action(CAP, "open_sig");
        let open_header = solana_mpp::format_authorization(&solana_mpp::PaymentCredential::new(
            challenge.to_echo(),
            serde_json::to_value(open_action).unwrap(),
        ))
        .unwrap();
        let SessionOutcome::Active(_) = session.process(&open_header).await.unwrap() else {
            panic!("expected open outcome");
        };

        let directive = session
            .server
            .begin_delivery(solana_mpp::server::session::DeliveryRequest::new(
                active.channel_id_str(),
                60,
            ))
            .await
            .unwrap();
        let voucher = active.prepare_increment(60).await.unwrap();
        let commit_action = SessionAction::Commit(solana_mpp::CommitPayload {
            delivery_id: directive.delivery_id.clone(),
            voucher,
        });
        let commit_header = solana_mpp::format_authorization(&solana_mpp::PaymentCredential::new(
            challenge.to_echo(),
            serde_json::to_value(commit_action).unwrap(),
        ))
        .unwrap();

        let SessionOutcome::Commit(receipt) = session.process(&commit_header).await.unwrap() else {
            panic!("expected commit outcome");
        };
        assert_eq!(receipt.delivery_id, directive.delivery_id);
        assert_eq!(receipt.amount, "60");
        assert_eq!(receipt.cumulative, "60");
    }

    #[tokio::test]
    async fn challenge_header_formats_session_challenge() {
        let header = test_session_mpp().challenge_header(CAP).unwrap();
        let challenge = solana_mpp::parse_www_authenticate(&header).unwrap();
        assert_eq!(challenge.intent.as_str(), INTENT);
        assert_eq!(challenge.method.as_str(), METHOD);
    }

    #[tokio::test]
    async fn finalize_params_returns_error_for_unknown_channel() {
        let err = test_session_mpp()
            .finalize_params("missing-channel")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to get finalize params"));
    }

    #[tokio::test]
    async fn run_pull_setup_skips_when_chain_not_configured() {
        let session = test_session_mpp();
        session.run_pull_setup(&no_tx_payload(CAP)).await.unwrap();
    }

    #[tokio::test]
    async fn run_pull_setup_rejects_invalid_pull_deposit() {
        let session = test_session_mpp().with_multi_delegate_chain(Box::new(chain_no_pda()));
        let mut payload = init_tx_payload(CAP);
        payload.approved_amount = Some("not-a-number".to_string());
        let err = session.run_pull_setup(&payload).await.unwrap_err();
        assert!(err.to_string().contains("pull open"));
    }

    fn payment_channel_payload(
        session: &SessionMpp,
        payer: solana_pubkey::Pubkey,
        authorized_signer: solana_pubkey::Pubkey,
        salt: u64,
    ) -> OpenPayload {
        let payee = solana_pubkey::Pubkey::try_from(session.session_config.recipient.as_str())
            .expect("valid test payee");
        let mint = solana_pubkey::Pubkey::try_from(session.session_config.currency.as_str())
            .expect("valid test mint");
        let program_id = session
            .session_config
            .program_id
            .unwrap_or_else(solana_mpp::program::payment_channels::default_program_id);
        let token_program = spl_token_program();
        let params = solana_mpp::program::payment_channels::OpenChannelParams {
            payer,
            payee,
            mint,
            authorized_signer,
            salt,
            deposit: CAP,
            grace_period: 900,
            recipients: vec![],
            token_program,
            program_id,
        };
        let channel = solana_mpp::program::payment_channels::derive_channel_addresses(&params)
            .channel
            .to_string();

        OpenPayload::payment_channel(
            channel,
            CAP.to_string(),
            payer.to_string(),
            payee.to_string(),
            mint.to_string(),
            salt,
            900,
            authorized_signer.to_string(),
            "pending".to_string(),
        )
        .with_transaction("tx".to_string())
    }

    #[test]
    fn payment_channel_open_params_validate_challenge_fields() {
        let session = test_session_mpp();
        let payload = payment_channel_payload(
            &session,
            solana_pubkey::Pubkey::new_unique(),
            solana_pubkey::Pubkey::new_unique(),
            42,
        );

        let params = session.payment_channel_open_params(&payload).unwrap();
        assert_eq!(params.deposit, CAP);
        assert_eq!(params.grace_period, 900);

        let mut tampered = payload.clone();
        tampered.payee = Some(solana_pubkey::Pubkey::new_unique().to_string());
        let err = session.payment_channel_open_params(&tampered).unwrap_err();
        assert!(err.to_string().contains("payee"));
    }

    #[test]
    fn transaction_contains_expected_payment_channel_open_instruction() {
        let session = test_session_mpp();
        let payload = payment_channel_payload(
            &session,
            solana_pubkey::Pubkey::new_unique(),
            solana_pubkey::Pubkey::new_unique(),
            7,
        );
        let expected = session
            .expected_payment_channel_open_instruction(&payload)
            .unwrap();
        let fee_payer = solana_pubkey::Pubkey::new_unique();
        let message = solana_message::Message::new_with_blockhash(
            std::slice::from_ref(&expected),
            Some(&fee_payer),
            &solana_hash::Hash::default(),
        );
        let tx = solana_transaction::Transaction::new_unsigned(message);
        assert!(transaction_contains_instruction(&tx, &expected));

        let mut tampered = expected.clone();
        tampered.data.push(99);
        assert!(!transaction_contains_instruction(&tx, &tampered));
    }

    #[test]
    fn validate_payment_channel_open_transaction_rejects_extra_instructions() {
        let session = test_session_mpp();
        let payload = payment_channel_payload(
            &session,
            solana_pubkey::Pubkey::new_unique(),
            solana_pubkey::Pubkey::new_unique(),
            11,
        );
        let expected = session
            .expected_payment_channel_open_instruction(&payload)
            .unwrap();
        let fee_payer = solana_pubkey::Pubkey::new_unique();
        let extra = solana_instruction::Instruction {
            program_id: solana_pubkey::Pubkey::new_unique(),
            accounts: vec![],
            data: vec![1],
        };
        let message = solana_message::Message::new_with_blockhash(
            &[expected.clone(), extra],
            Some(&fee_payer),
            &solana_hash::Hash::default(),
        );
        let tx = solana_transaction::Transaction::new_unsigned(message);

        let err =
            validate_payment_channel_open_transaction(&tx, &expected, &fee_payer).unwrap_err();
        assert!(err.to_string().contains("exactly one instruction"));
    }

    #[test]
    fn validate_payment_channel_open_transaction_rejects_wrong_fee_payer() {
        let session = test_session_mpp();
        let payload = payment_channel_payload(
            &session,
            solana_pubkey::Pubkey::new_unique(),
            solana_pubkey::Pubkey::new_unique(),
            12,
        );
        let expected = session
            .expected_payment_channel_open_instruction(&payload)
            .unwrap();
        let fee_payer = solana_pubkey::Pubkey::new_unique();
        let message = solana_message::Message::new_with_blockhash(
            std::slice::from_ref(&expected),
            Some(&fee_payer),
            &solana_hash::Hash::default(),
        );
        let tx = solana_transaction::Transaction::new_unsigned(message);

        let wrong_fee_payer = solana_pubkey::Pubkey::new_unique();
        let err = validate_payment_channel_open_transaction(&tx, &expected, &wrong_fee_payer)
            .unwrap_err();
        assert!(err.to_string().contains("fee payer"));
    }
}
