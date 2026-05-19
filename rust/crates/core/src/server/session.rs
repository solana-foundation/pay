//! Server-side session intent — channel lifecycle and voucher verification.
//!
//! Wraps [`solana_mpp::server::session::SessionServer`] with an in-memory
//! channel store and provides challenge issuance + action dispatch that fits
//! the pay-core middleware pattern.
//!
//! # Pull-mode session flow
//!
//! ```text
//! Client sends `open` with deterministic payment-channel fields
//!   │
//!   ▼
//! Server validates the fields against the challenge and opens the channel
//!   │
//!   ▼
//! Server records channel state; the client signs vouchers for that channel
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
    SessionPullVoucherStrategy, parse_authorization,
};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::{Error, Result};

const INTENT: &str = "session";
const METHOD: &str = "solana";
const DEFAULT_REALM: &str = "MPP Session";
const DEFAULT_BATCH_OPEN_INTERVAL_MS: u64 = 400;
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

fn session_close_already_requested(error: &solana_mpp::Error) -> bool {
    error.to_string().contains("Close already requested")
}

fn session_close_already_finalized(error: &solana_mpp::Error) -> bool {
    error.to_string().contains("already finalized")
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
#[derive(Debug)]
pub enum SessionOutcome {
    /// `open` or `topup` — channel state after the action.
    Active(ChannelState),
    /// `voucher` accepted — channel id + new settled cumulative (base units).
    Voucher { channel_id: String, cumulative: u64 },
    /// `commit` accepted — receipt for the metered delivery.
    Commit(CommitReceipt),
    /// `close` accepted — `FinalizeParams` carries what's needed to submit the
    /// on-chain finalize + distribute transactions.
    Closed {
        params: FinalizeParams,
        signature: Option<String>,
    },
}

pub type OpenChannelBatch = Vec<(String, String, u64)>;
type OpenChannelBatchFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
type OpenChannelBatchSubmitter =
    Arc<dyn Fn(OpenChannelBatch) -> OpenChannelBatchFuture + Send + Sync>;

#[derive(Clone)]
struct SessionOperatorRuntime {
    server: Arc<SessionServer<MemoryChannelStore>>,
    rpc_url: Option<String>,
    payment_channel_signer: Arc<Mutex<Option<Arc<dyn SolanaSigner>>>>,
    payment_channel_payer_signer: Arc<Mutex<Option<Arc<dyn SolanaSigner>>>>,
    committed_watermarks: Arc<Mutex<HashMap<String, u64>>>,
    open_channel_batcher: Arc<Mutex<Option<OpenChannelBatchSubmitter>>>,
}

impl SessionOperatorRuntime {
    fn record_committed_watermark(&self, session_id: impl Into<String>, cumulative: u64) {
        if let Ok(mut watermarks) = self.committed_watermarks.lock() {
            let session_id = session_id.into();
            let entry = watermarks.entry(session_id).or_default();
            *entry = (*entry).max(cumulative);
        }
    }

    fn payment_channel_signer(&self) -> Option<Arc<dyn SolanaSigner>> {
        self.payment_channel_signer
            .lock()
            .ok()
            .and_then(|signer| signer.clone())
    }

    fn payment_channel_payer_signer(&self) -> Option<Arc<dyn SolanaSigner>> {
        self.payment_channel_payer_signer
            .lock()
            .ok()
            .and_then(|signer| signer.clone())
            .or_else(|| self.payment_channel_signer())
    }

    fn open_channel_batcher(&self) -> Option<OpenChannelBatchSubmitter> {
        self.open_channel_batcher
            .lock()
            .ok()
            .and_then(|batcher| batcher.clone())
    }

    async fn operator_close_channel(&self, channel_id: &str) -> Result<SessionCloseResult> {
        use solana_mpp::ClosePayload;

        if self.channel_is_tombstoned_on_chain(channel_id).await {
            self.server
                .mark_finalized(channel_id)
                .await
                .map_err(|e| Error::Mpp(format!("Failed to mark session finalized: {e}")))?;
            return Ok(SessionCloseResult::AlreadyFinalized);
        }

        let payload = ClosePayload {
            channel_id: channel_id.to_string(),
            voucher: None,
        };
        let params = match self.server.process_close(&payload).await {
            Ok(params) => params,
            Err(error) if session_close_already_finalized(&error) => {
                return Ok(SessionCloseResult::AlreadyFinalized);
            }
            Err(error) if session_close_already_requested(&error) => self
                .server
                .finalize_params(channel_id)
                .await
                .map_err(|e| Error::Mpp(format!("Failed to get finalize params: {e}")))?,
            Err(error) => {
                return Err(Error::Mpp(format!("Session auto-close failed: {error}")));
            }
        };

        self.record_committed_watermark(params.channel_id.to_string(), params.settled);
        let settlement = self.submit_payment_channel_settlement(&params).await;
        let signature = match settlement {
            Ok(signature) => signature,
            Err(_error) if self.channel_is_tombstoned_on_chain(channel_id).await => {
                self.server
                    .mark_finalized(channel_id)
                    .await
                    .map_err(|e| Error::Mpp(format!("Failed to mark session finalized: {e}")))?;
                return Ok(SessionCloseResult::AlreadyFinalized);
            }
            Err(error) => return Err(error),
        };
        if let Some(signature) = signature {
            self.server
                .mark_finalized(&params.channel_id.to_string())
                .await
                .map_err(|e| Error::Mpp(format!("Failed to mark session finalized: {e}")))?;
            tracing::info!(
                %signature,
                channel = %params.channel_id,
                "payment-channel settlement confirmed"
            );
        }

        Ok(SessionCloseResult::Closed {
            settled: params.settled,
        })
    }

    async fn channel_is_tombstoned_on_chain(&self, channel_id: &str) -> bool {
        let Some(rpc_url) = self.rpc_url.clone() else {
            return false;
        };
        let Ok(channel) = solana_pubkey::Pubkey::from_str(channel_id) else {
            return false;
        };

        use solana_mpp::solana_rpc_client::nonblocking::rpc_client::RpcClient;
        RpcClient::new(rpc_url)
            .get_account(&channel)
            .await
            .map(|account| account.data.as_slice() == [2])
            .unwrap_or(false)
    }

    async fn submit_payment_channel_settlement(
        &self,
        params: &FinalizeParams,
    ) -> Result<Option<String>> {
        let Some(signer) = self.payment_channel_signer() else {
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
            Arc::clone(&signer),
            rpc_url,
            &mut tx,
            "payment-channel settlement",
        )
        .await
        .map(Some)
    }

    async fn flush_open_channel_batch(&self, batch: OpenChannelBatch) -> Result<()> {
        let Some(submitter) = self.open_channel_batcher() else {
            return Ok(());
        };
        submitter(batch).await
    }
}

#[derive(Clone)]
struct SessionLifecycleHandle {
    tx: mpsc::UnboundedSender<SessionLifecycleCommand>,
}

impl SessionLifecycleHandle {
    fn send(&self, command: SessionLifecycleCommand) {
        if self.tx.send(command).is_err() {
            tracing::debug!("session lifecycle runloop is not accepting events");
        }
    }
}

#[derive(Debug)]
enum SessionLifecycleCommand {
    ConfigureCloseDelay {
        close_delay: Option<Duration>,
    },
    ConfigureOpenBatchInterval {
        interval: Duration,
    },
    RecordOpen {
        owner: String,
        token_account: String,
        cap: u64,
    },
    Touch {
        channel_id: String,
    },
    Remove {
        channel_id: String,
    },
}

struct SessionLifecycleRunloop {
    runtime: SessionOperatorRuntime,
    close_delay: Option<Duration>,
    open_batch_interval: Duration,
    rx: mpsc::UnboundedReceiver<SessionLifecycleCommand>,
    deadlines: HashMap<String, Instant>,
    next_open_flush: Option<Instant>,
    pending_opens: OpenChannelBatch,
}

impl SessionLifecycleRunloop {
    fn new(
        runtime: SessionOperatorRuntime,
        open_batch_interval: Duration,
        rx: mpsc::UnboundedReceiver<SessionLifecycleCommand>,
    ) -> Self {
        Self {
            runtime,
            close_delay: None,
            open_batch_interval,
            rx,
            deadlines: HashMap::new(),
            next_open_flush: None,
            pending_opens: Vec::new(),
        }
    }

    async fn run(mut self) {
        loop {
            if let Some(deadline) = self.next_wakeup() {
                tokio::select! {
                    command = self.rx.recv() => {
                        if !self.handle_command(command) {
                            break;
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        self.flush_due_open_batch().await;
                        self.close_due_channels().await;
                    }
                }
            } else {
                let command = self.rx.recv().await;
                if !self.handle_command(command) {
                    break;
                }
            }
        }
    }

    fn handle_command(&mut self, command: Option<SessionLifecycleCommand>) -> bool {
        match command {
            Some(SessionLifecycleCommand::ConfigureCloseDelay { close_delay }) => {
                self.close_delay = close_delay;
                if self.close_delay.is_none() {
                    self.deadlines.clear();
                }
                true
            }
            Some(SessionLifecycleCommand::ConfigureOpenBatchInterval { interval }) => {
                self.open_batch_interval = interval;
                true
            }
            Some(SessionLifecycleCommand::RecordOpen {
                owner,
                token_account,
                cap,
            }) => {
                self.pending_opens.push((owner, token_account, cap));
                if self.next_open_flush.is_none() {
                    self.next_open_flush = Some(Instant::now() + self.open_batch_interval);
                }
                true
            }
            Some(SessionLifecycleCommand::Touch { channel_id }) => {
                if let Some(close_delay) = self.close_delay {
                    let deadline = Instant::now() + close_delay;
                    self.deadlines.insert(channel_id, deadline);
                }
                true
            }
            Some(SessionLifecycleCommand::Remove { channel_id }) => {
                self.deadlines.remove(&channel_id);
                true
            }
            None => false,
        }
    }

    fn next_wakeup(&self) -> Option<Instant> {
        self.deadlines
            .values()
            .copied()
            .chain(self.next_open_flush)
            .min()
    }

    async fn flush_due_open_batch(&mut self) {
        let Some(deadline) = self.next_open_flush else {
            return;
        };
        if deadline > Instant::now() {
            return;
        }

        self.next_open_flush = None;
        if self.pending_opens.is_empty() {
            return;
        }

        let batch = std::mem::take(&mut self.pending_opens);
        let batch_len = batch.len();
        if let Err(error) = self.runtime.flush_open_channel_batch(batch).await {
            tracing::warn!(
                error = %error,
                batch_len,
                "operator open-channel batch failed"
            );
        }
    }

    async fn close_due_channels(&mut self) {
        let now = Instant::now();
        let due = self
            .deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(channel_id, _)| channel_id.clone())
            .collect::<Vec<_>>();

        for channel_id in due {
            self.deadlines.remove(&channel_id);
            match self.runtime.operator_close_channel(&channel_id).await {
                Ok(SessionCloseResult::Closed { settled }) => {
                    tracing::info!(channel_id, settled, "operator auto-closed payment channel");
                }
                Ok(SessionCloseResult::AlreadyFinalized) => {
                    tracing::debug!(channel_id, "payment channel already finalized");
                }
                Err(error) => {
                    tracing::warn!(
                        channel_id,
                        error = %error,
                        "operator auto-close failed; retrying after delay"
                    );
                    if let Some(close_delay) = self.close_delay {
                        self.deadlines
                            .insert(channel_id, Instant::now() + close_delay);
                    }
                }
            }
        }
    }
}

enum SessionCloseResult {
    Closed { settled: u64 },
    AlreadyFinalized,
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
    server: Arc<SessionServer<MemoryChannelStore>>,
    session_config: SessionConfig,
    secret_key: String,
    realm: String,
    rpc_url: Option<String>,
    payment_channel_signer: Arc<Mutex<Option<Arc<dyn SolanaSigner>>>>,
    payment_channel_payer_signer: Arc<Mutex<Option<Arc<dyn SolanaSigner>>>>,
    committed_watermarks: Arc<Mutex<HashMap<String, u64>>>,
    pull_sessions: Arc<Mutex<HashSet<String>>>,
    open_channel_batcher: Arc<Mutex<Option<OpenChannelBatchSubmitter>>>,
    lifecycle: SessionLifecycleHandle,
    operator_runtime: SessionOperatorRuntime,
    pull_voucher_strategy: PullVoucherStrategy,
    /// Interface to on-chain multi-delegate state (optional; pull-mode setup
    /// is required for operated-voucher pull setup).
    multi_delegate_chain: Option<Box<dyn MultiDelegateChain>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PullVoucherStrategy {
    #[default]
    Disabled,
    ClientVoucher,
    OperatedVoucher,
}

impl SessionMpp {
    /// Create from a [`SessionConfig`] and an HMAC secret key.
    pub fn new(config: SessionConfig, secret_key: impl Into<String>) -> Self {
        let session_config = config.clone();
        let server = Arc::new(SessionServer::new(config, MemoryChannelStore::new()));
        let payment_channel_signer = Arc::new(Mutex::new(None));
        let payment_channel_payer_signer = Arc::new(Mutex::new(None));
        let committed_watermarks = Arc::new(Mutex::new(HashMap::new()));
        let pull_sessions = Arc::new(Mutex::new(HashSet::new()));
        let open_channel_batcher = Arc::new(Mutex::new(None));
        let operator_runtime = SessionOperatorRuntime {
            server: Arc::clone(&server),
            rpc_url: session_config.rpc_url.clone(),
            payment_channel_signer: Arc::clone(&payment_channel_signer),
            payment_channel_payer_signer: Arc::clone(&payment_channel_payer_signer),
            committed_watermarks: Arc::clone(&committed_watermarks),
            open_channel_batcher: Arc::clone(&open_channel_batcher),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let runloop = SessionLifecycleRunloop::new(
            operator_runtime.clone(),
            Duration::from_millis(DEFAULT_BATCH_OPEN_INTERVAL_MS),
            rx,
        );
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(runloop.run());
        } else {
            tracing::debug!("session lifecycle runloop not started; no tokio runtime is active");
        }

        Self {
            rpc_url: session_config.rpc_url.clone(),
            server,
            session_config,
            secret_key: secret_key.into(),
            realm: DEFAULT_REALM.to_string(),
            payment_channel_signer,
            payment_channel_payer_signer,
            committed_watermarks,
            pull_sessions,
            open_channel_batcher,
            lifecycle: SessionLifecycleHandle { tx },
            operator_runtime,
            pull_voucher_strategy: PullVoucherStrategy::Disabled,
            multi_delegate_chain: None,
        }
    }

    pub fn with_realm(mut self, realm: impl Into<String>) -> Self {
        self.realm = realm.into();
        self
    }

    /// Wire up on-chain multi-delegate state resolution for pull-mode sessions.
    ///
    /// This enables the operated-voucher pull path. When set, every
    /// operated-voucher pull-mode `open` will:
    /// 1. Fetch the client's `MultiDelegate` + `FixedDelegation` state.
    /// 2. Submit a setup tx if the delegation is missing or insufficient.
    pub fn with_multi_delegate_chain(mut self, chain: Box<dyn MultiDelegateChain>) -> Self {
        self.pull_voucher_strategy = PullVoucherStrategy::OperatedVoucher;
        self.multi_delegate_chain = Some(chain);
        self
    }

    pub fn with_pull_voucher_strategy(mut self, strategy: PullVoucherStrategy) -> Self {
        self.pull_voucher_strategy = strategy;
        self
    }

    /// Configure the operator signer used to co-sign client-provided
    /// payment-channel open transactions and to submit close settlement txs.
    pub fn with_payment_channel_signer(self, signer: Arc<dyn SolanaSigner>) -> Self {
        if let Ok(mut payment_channel_signer) = self.payment_channel_signer.lock() {
            *payment_channel_signer = Some(signer);
        }
        self
    }

    /// Configure the signer that funds server-opened payment channels.
    ///
    /// When omitted, the settlement signer is reused for backwards
    /// compatibility. Server-opened client-voucher sessions normally set this
    /// to a distinct funded payer because the payment-channel program rejects
    /// `payer == payee`.
    pub fn with_payment_channel_payer_signer(self, signer: Arc<dyn SolanaSigner>) -> Self {
        if let Ok(mut payment_channel_payer_signer) = self.payment_channel_payer_signer.lock() {
            *payment_channel_payer_signer = Some(signer);
        }
        self
    }

    /// Start the single operator-side lifecycle runloop for delayed channel close.
    ///
    /// The runloop is intentionally centralized: request handlers only record
    /// activity, while this task owns the close/settle/distribute sequence.
    pub fn start_lifecycle_runloop(&self, close_delay: Duration) {
        if close_delay.is_zero() {
            self.lifecycle
                .send(SessionLifecycleCommand::ConfigureCloseDelay { close_delay: None });
            tracing::info!("session auto-close disabled");
            return;
        }

        self.lifecycle
            .send(SessionLifecycleCommand::ConfigureCloseDelay {
                close_delay: Some(close_delay),
            });
        tracing::info!(
            close_delay_ms = close_delay.as_millis(),
            "started session lifecycle runloop"
        );
    }

    pub fn set_open_channel_batch_interval(&self, interval: Duration) {
        self.lifecycle
            .send(SessionLifecycleCommand::ConfigureOpenBatchInterval { interval });
    }

    pub fn with_test_open_channel_batcher<F, Fut>(self, batcher: F) -> Self
    where
        F: Fn(OpenChannelBatch) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.with_test_open_channel_batcher_interval(DEFAULT_BATCH_OPEN_INTERVAL_MS, batcher)
    }

    pub fn with_test_open_channel_batcher_interval<F, Fut>(
        self,
        batch_open_interval_ms: u64,
        batcher: F,
    ) -> Self
    where
        F: Fn(OpenChannelBatch) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let batcher = Arc::new(batcher);
        let submitter: OpenChannelBatchSubmitter = Arc::new(move |batch| {
            let batcher = Arc::clone(&batcher);
            Box::pin(async move { batcher(batch).await })
        });
        if let Ok(mut open_channel_batcher) = self.open_channel_batcher.lock() {
            *open_channel_batcher = Some(submitter);
        }
        self.lifecycle
            .send(SessionLifecycleCommand::ConfigureOpenBatchInterval {
                interval: Duration::from_millis(batch_open_interval_ms),
            });
        self
    }

    /// Token decimals for base-unit settlement amounts.
    pub fn decimals(&self) -> u8 {
        self.session_config.decimals
    }

    /// Minimum accepted voucher increment in base units.
    pub fn min_voucher_delta(&self) -> u64 {
        self.session_config.min_voucher_delta
    }

    /// Record channel activity so the lifecycle runloop can defer auto-close.
    pub fn touch_channel(&self, channel_id: impl Into<String>) {
        let channel_id = channel_id.into();
        if self
            .pull_sessions
            .lock()
            .map(|sessions| sessions.contains(&channel_id))
            .unwrap_or(false)
        {
            return;
        }
        self.lifecycle
            .send(SessionLifecycleCommand::Touch { channel_id });
    }

    /// Latest cumulative watermark accepted by this process for a session.
    pub fn committed_watermark(&self, session_id: &str) -> Option<u64> {
        self.committed_watermarks
            .lock()
            .ok()
            .and_then(|watermarks| watermarks.get(session_id).copied())
    }

    /// Build a [`PaymentChallenge`] for a new session with the given cap.
    pub fn challenge(&self, cap: u64) -> Result<PaymentChallenge> {
        let mut request = self.server.build_challenge_request(cap);
        match self.pull_voucher_strategy {
            PullVoucherStrategy::Disabled => {
                request.modes.retain(|mode| mode != &SessionMode::Pull);
                request.pull_voucher_strategy = None;
            }
            PullVoucherStrategy::ClientVoucher => {
                if request.modes.contains(&SessionMode::Pull) {
                    request.pull_voucher_strategy = Some(SessionPullVoucherStrategy::ClientVoucher);
                }
            }
            PullVoucherStrategy::OperatedVoucher => {
                if request.modes.contains(&SessionMode::Pull) {
                    request.pull_voucher_strategy =
                        Some(SessionPullVoucherStrategy::OperatedVoucher);
                }
            }
        }
        if request.modes == [SessionMode::Push] {
            request.modes.clear();
        }
        request.recent_blockhash = self.prefetch_latest_blockhash();
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
    /// For payment-channel `open` actions, the server either co-signs a
    /// client-provided open transaction or opens the channel itself from its
    /// configured payment-channel signer, then stores the confirmed channel.
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
                let client_voucher_pull = p.mode == SessionMode::Pull
                    && self.pull_voucher_strategy == PullVoucherStrategy::ClientVoucher;
                if p.mode == SessionMode::Pull {
                    self.process_pull_open(p).await?;
                }

                let mut submitted_open = None;
                let open_payload;
                let payload_for_open = if client_voucher_pull {
                    let signature = if p.transaction.is_some() {
                        self.submit_payment_channel_open(p).await?.ok_or_else(|| {
                            Error::Mpp(
                                "client-voucher pull open transaction was not submitted"
                                    .to_string(),
                            )
                        })?
                    } else {
                        self.submit_server_payment_channel_open(p).await?
                    };
                    open_payload = {
                        let mut payload = p.clone();
                        payload.signature = signature.clone();
                        payload
                    };
                    submitted_open = Some(signature);
                    &open_payload
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

                if p.mode == SessionMode::Pull && !client_voucher_pull {
                    self.record_pull_session(state.channel_id.clone());
                }
                self.record_committed_watermark(state.channel_id.clone(), state.cumulative);
                self.record_open_channel_batch(p);
                self.touch_channel(state.channel_id.clone());
                Ok(SessionOutcome::Active(state))
            }

            SessionAction::Voucher(p) => {
                let cumulative = self
                    .server
                    .verify_voucher(p)
                    .await
                    .map_err(|e| Error::PaymentRejected(e.to_string()))?;
                let channel_id = p.voucher.data.channel_id.clone();
                self.record_committed_watermark(channel_id.clone(), cumulative);
                self.touch_channel(channel_id.clone());
                Ok(SessionOutcome::Voucher {
                    channel_id,
                    cumulative,
                })
            }

            SessionAction::Commit(p) => {
                let receipt = self
                    .server
                    .process_commit(p)
                    .await
                    .map_err(|e| Error::PaymentRejected(e.to_string()))?;
                if let Ok(cumulative) = receipt.cumulative.parse::<u64>() {
                    self.record_committed_watermark(receipt.session_id.clone(), cumulative);
                }
                self.touch_channel(receipt.session_id.clone());
                Ok(SessionOutcome::Commit(receipt))
            }

            SessionAction::TopUp(p) => {
                let state = self
                    .server
                    .process_topup(p)
                    .await
                    .map_err(|e| Error::Mpp(format!("TopUp failed: {e}")))?;
                self.record_committed_watermark(state.channel_id.clone(), state.cumulative);
                self.touch_channel(state.channel_id.clone());
                Ok(SessionOutcome::Active(state))
            }

            SessionAction::Close(p) => {
                let params = self
                    .server
                    .process_close(p)
                    .await
                    .map_err(|e| Error::Mpp(format!("Session close failed: {e}")))?;
                self.record_committed_watermark(params.channel_id.to_string(), params.settled);
                let settlement = self.submit_payment_channel_settlement(&params).await;
                let signature = match settlement {
                    Ok(signature) => signature,
                    Err(_error)
                        if self
                            .operator_runtime
                            .channel_is_tombstoned_on_chain(&params.channel_id.to_string())
                            .await =>
                    {
                        self.server
                            .mark_finalized(&params.channel_id.to_string())
                            .await
                            .map_err(|e| {
                                Error::Mpp(format!("Failed to mark session finalized: {e}"))
                            })?;
                        None
                    }
                    Err(error) => return Err(error),
                };
                if let Some(signature) = signature.as_ref() {
                    self.server
                        .mark_finalized(&params.channel_id.to_string())
                        .await
                        .map_err(|e| {
                            Error::Mpp(format!("Failed to mark session finalized: {e}"))
                        })?;
                    tracing::info!(%signature, channel = %params.channel_id, "payment-channel settlement confirmed");
                }
                self.unschedule_channel_close(params.channel_id.to_string());
                Ok(SessionOutcome::Closed { params, signature })
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
        let session_id = request.session_id.clone();
        let directive = self
            .server
            .begin_delivery(request)
            .await
            .map_err(|e| Error::Mpp(format!("Failed to reserve session delivery: {e}")))?;
        self.touch_channel(session_id);
        Ok(directive)
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    fn unschedule_channel_close(&self, channel_id: impl Into<String>) {
        self.lifecycle.send(SessionLifecycleCommand::Remove {
            channel_id: channel_id.into(),
        });
    }

    fn record_open_channel_batch(&self, payload: &OpenPayload) {
        if payload.mode != SessionMode::Pull
            || self.pull_voucher_strategy != PullVoucherStrategy::OperatedVoucher
        {
            return;
        }

        let Some(owner) = payload.owner.clone() else {
            tracing::debug!("pull open missing owner; skipping open-channel batch record");
            return;
        };
        let Some(token_account) = payload.token_account.clone() else {
            tracing::debug!("pull open missing token account; skipping open-channel batch record");
            return;
        };
        let Ok(cap) = payload.deposit_amount() else {
            tracing::debug!("pull open missing cap; skipping open-channel batch record");
            return;
        };

        self.lifecycle.send(SessionLifecycleCommand::RecordOpen {
            owner,
            token_account,
            cap,
        });
    }

    fn record_committed_watermark(&self, session_id: impl Into<String>, cumulative: u64) {
        self.operator_runtime
            .record_committed_watermark(session_id, cumulative);
    }

    fn record_pull_session(&self, session_id: impl Into<String>) {
        if let Ok(mut sessions) = self.pull_sessions.lock() {
            sessions.insert(session_id.into());
        }
    }

    /// Validate and prepare a pull-mode open.
    async fn process_pull_open(&self, payload: &OpenPayload) -> Result<()> {
        match self.pull_voucher_strategy {
            PullVoucherStrategy::Disabled => Err(Error::Mpp(
                "pull-mode sessions are disabled; use push or configure pull_voucher_strategy"
                    .to_string(),
            )),
            PullVoucherStrategy::ClientVoucher => self.validate_client_voucher_pull_open(payload),
            PullVoucherStrategy::OperatedVoucher => self.run_pull_setup(payload).await,
        }
    }

    fn validate_client_voucher_pull_open(&self, payload: &OpenPayload) -> Result<()> {
        if payload.channel_id.is_none() || payload.deposit.is_none() {
            return Err(Error::Mpp(
                "client-voucher pull sessions require payment-channel channelId and deposit"
                    .to_string(),
            ));
        }
        if payload.token_account.is_some() || payload.approved_amount.is_some() {
            return Err(Error::Mpp(
                "client-voucher pull sessions do not use token-account delegation; use operated_voucher"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Run the multi-delegator pre-flight for an operated-voucher pull-mode open.
    async fn run_pull_setup(&self, payload: &OpenPayload) -> Result<()> {
        let chain = match &self.multi_delegate_chain {
            Some(c) => c.as_ref(),
            None => {
                return Err(Error::Mpp(
                    "operated-voucher pull sessions require a multi-delegate chain".to_string(),
                ));
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
        let signer = self
            .operator_runtime
            .payment_channel_signer()
            .ok_or_else(|| {
                Error::Mpp(
                    "payment-channel open transaction requires an operator signer".to_string(),
                )
            })?;
        let rpc_url = self.rpc_url.clone().ok_or_else(|| {
            Error::Mpp("payment-channel open transaction requires an RPC URL".to_string())
        })?;

        let mut tx = decode_base64_transaction(transaction)?;
        let expected = self.expected_payment_channel_open_instruction(payload)?;
        validate_payment_channel_open_transaction(&tx, &expected, &signer.pubkey())?;

        sign_and_submit_transaction(
            Arc::clone(&signer),
            rpc_url,
            &mut tx,
            "payment-channel open",
        )
        .await
        .map(Some)
    }

    async fn submit_server_payment_channel_open(&self, payload: &OpenPayload) -> Result<String> {
        let signer = self
            .operator_runtime
            .payment_channel_payer_signer()
            .ok_or_else(|| {
                Error::Mpp("server-opened payment channel requires an operator signer".to_string())
            })?;
        let rpc_url = self.rpc_url.clone().ok_or_else(|| {
            Error::Mpp("server-opened payment channel requires an RPC URL".to_string())
        })?;
        let params = self.payment_channel_open_params(payload)?;
        let fee_payer = signer.pubkey();
        if params.payer != fee_payer {
            return Err(Error::Mpp(
                "server-opened payment-channel payer must match operator signer".to_string(),
            ));
        }

        let instruction = solana_mpp::program::payment_channels::build_open_instruction(&params);
        let blockhash = fetch_latest_blockhash(&rpc_url)?;
        let message = solana_message::Message::new_with_blockhash(
            &[instruction],
            Some(&fee_payer),
            &blockhash,
        );
        let mut tx = solana_transaction::Transaction::new_unsigned(message);
        sign_and_submit_transaction(
            Arc::clone(&signer),
            rpc_url,
            &mut tx,
            "payment-channel open",
        )
        .await
    }

    async fn submit_payment_channel_settlement(
        &self,
        params: &FinalizeParams,
    ) -> Result<Option<String>> {
        self.operator_runtime
            .submit_payment_channel_settlement(params)
            .await
    }

    fn expected_payment_channel_open_instruction(
        &self,
        payload: &OpenPayload,
    ) -> Result<solana_instruction::Instruction> {
        self.server
            .payment_channel_open_instruction(payload)
            .map_err(|e| Error::Mpp(e.to_string()))
    }

    fn payment_channel_open_params(
        &self,
        payload: &OpenPayload,
    ) -> Result<solana_mpp::program::payment_channels::OpenChannelParams> {
        self.server
            .payment_channel_open_params(payload)
            .map_err(|e| Error::Mpp(e.to_string()))
    }

    /// Best-effort prefetch of the latest blockhash for session challenges.
    ///
    /// Calls the same RPC as [`fetch_latest_blockhash`] but swallows errors
    /// (logged at debug) since the challenge remains valid without this field.
    fn prefetch_latest_blockhash(&self) -> Option<String> {
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
    use solana_mpp::protocol::solana::programs;
    use std::str::FromStr;
    solana_pubkey::Pubkey::from_str(programs::TOKEN_PROGRAM).expect("valid SPL token program id")
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
        use solana_commitment_config::CommitmentConfig;
        use solana_mpp::solana_rpc_client::rpc_client::RpcClient;

        let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        let expected_signature =
            tx.signatures.first().copied().ok_or_else(|| {
                Error::Mpp(format!("{context} transaction is missing a signature"))
            })?;

        match rpc.send_transaction(&tx) {
            Ok(signature) => {
                wait_for_transaction_confirmation(&rpc, &signature, context)?;
                Ok(signature.to_string())
            }
            Err(send_error) => {
                match wait_for_transaction_confirmation(&rpc, &expected_signature, context) {
                    Ok(()) => {
                        tracing::warn!(
                            %expected_signature,
                            error = %send_error,
                            "{context} transaction confirmed after submit returned an error"
                        );
                        Ok(expected_signature.to_string())
                    }
                    Err(_) => Err(Error::Mpp(format!(
                        "{context} transaction submission failed: {send_error}"
                    ))),
                }
            }
        }
    })
    .await
    .map_err(|e| Error::Mpp(format!("spawn_blocking join error: {e}")))?
}

fn wait_for_transaction_confirmation(
    rpc: &solana_mpp::solana_rpc_client::rpc_client::RpcClient,
    signature: &solana_signature::Signature,
    context: &'static str,
) -> Result<()> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_status_error = None;
    while Instant::now() < deadline {
        match rpc.get_signature_status(signature) {
            Ok(Some(Ok(()))) => return Ok(()),
            Ok(Some(Err(error))) => {
                return Err(Error::Mpp(format!("{context} transaction failed: {error}")));
            }
            Ok(None) => {}
            Err(error) => {
                last_status_error = Some(error.to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let detail = last_status_error
        .map(|error| format!("; last status error: {error}"))
        .unwrap_or_default();
    Err(Error::Mpp(format!(
        "{context} transaction was not confirmed before timeout{detail}"
    )))
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
    fn prefetch_latest_blockhash_without_rpc_returns_none() {
        assert_eq!(test_session_mpp().prefetch_latest_blockhash(), None);
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
            .expect_err("non-session intent should error");
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
            .expect_err("invalid auth should error");
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
            .expect_err("unknown action should error");
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
        assert_eq!(session.committed_watermark(&opened.channel_id), Some(0));

        let voucher_header = handle.voucher_header(75).await.unwrap();
        let SessionOutcome::Voucher { cumulative, .. } =
            session.process(&voucher_header).await.unwrap()
        else {
            panic!("expected voucher outcome");
        };
        assert_eq!(cumulative, 75);
        assert_eq!(session.committed_watermark(&opened.channel_id), Some(75));

        let topup_header = handle.topup_header(CAP + 500, "topup_sig").await.unwrap();
        let SessionOutcome::Active(topped_up) = session.process(&topup_header).await.unwrap()
        else {
            panic!("expected topup outcome");
        };
        assert_eq!(topped_up.deposit, CAP + 500);

        let close_header = handle.close_header(Some(25)).await.unwrap();
        let SessionOutcome::Closed { params, signature } =
            session.process(&close_header).await.unwrap()
        else {
            panic!("expected close outcome");
        };
        assert_eq!(params.settled, 100);
        assert_eq!(signature, None);
        assert_eq!(session.committed_watermark(&opened.channel_id), Some(100));
    }

    #[tokio::test]
    async fn lifecycle_runloop_batches_pull_channel_opens() {
        let batches: Arc<Mutex<Vec<OpenChannelBatch>>> = Arc::new(Mutex::new(Vec::new()));
        let session = test_session_mpp()
            .with_multi_delegate_chain(Box::new(chain_sufficient(CAP)))
            .with_test_open_channel_batcher_interval(10, {
                let batches = Arc::clone(&batches);
                move |batch| {
                    let batches = Arc::clone(&batches);
                    async move {
                        batches.lock().unwrap().push(batch);
                        Ok(())
                    }
                }
            });

        let challenge = session.challenge(CAP).unwrap();
        let owner = solana_pubkey::Pubkey::new_unique().to_string();
        let token_account = solana_pubkey::Pubkey::new_unique().to_string();
        let payload = OpenPayload::pull(
            token_account.clone(),
            CAP.to_string(),
            owner.clone(),
            solana_pubkey::Pubkey::new_unique().to_string(),
            "open_sig".to_string(),
        );
        let credential = PaymentCredential::new(
            challenge.to_echo(),
            serde_json::to_value(SessionAction::Open(payload)).unwrap(),
        );
        let auth_header = format_authorization(&credential).unwrap();

        let SessionOutcome::Active(_) = session.process(&auth_header).await.unwrap() else {
            panic!("expected pull open to return active session");
        };
        tokio::time::sleep(Duration::from_millis(40)).await;

        assert_eq!(
            batches.lock().unwrap().clone(),
            vec![vec![(owner, token_account, CAP)]]
        );
    }

    #[tokio::test]
    async fn client_voucher_pull_uses_payment_channel_payload_shape() {
        let session =
            test_session_mpp().with_pull_voucher_strategy(PullVoucherStrategy::ClientVoucher);
        let mut payload = payment_channel_payload(
            &session,
            solana_pubkey::Pubkey::new_unique(),
            solana_pubkey::Pubkey::new_unique(),
            42,
        );
        payload.mode = SessionMode::Pull;

        session.process_pull_open(&payload).await.unwrap();
    }

    #[tokio::test]
    async fn client_voucher_pull_accepts_server_opened_payment_channel_shape() {
        let session =
            test_session_mpp().with_pull_voucher_strategy(PullVoucherStrategy::ClientVoucher);
        let mut payload = payment_channel_payload(
            &session,
            solana_pubkey::Pubkey::new_unique(),
            solana_pubkey::Pubkey::new_unique(),
            43,
        );
        payload.mode = SessionMode::Pull;
        payload.transaction = None;

        session.process_pull_open(&payload).await.unwrap();
    }

    #[tokio::test]
    async fn client_voucher_pull_rejects_delegated_token_payload_shape() {
        let session =
            test_session_mpp().with_pull_voucher_strategy(PullVoucherStrategy::ClientVoucher);
        let payload = OpenPayload::pull(
            solana_pubkey::Pubkey::new_unique().to_string(),
            CAP.to_string(),
            solana_pubkey::Pubkey::new_unique().to_string(),
            solana_pubkey::Pubkey::new_unique().to_string(),
            "open_sig".to_string(),
        );

        let err = session.process_pull_open(&payload).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("client-voucher pull sessions require payment-channel")
        );
    }

    #[tokio::test]
    async fn server_opened_payment_channel_requires_operator_payer() {
        let signer: Arc<dyn SolanaSigner> = Arc::from(test_session_signer());
        let mut config = test_session_config();
        config.operator = signer.pubkey().to_string();
        config.rpc_url = Some("http://127.0.0.1:8899".to_string());
        let session = SessionMpp::new(config, "test-secret")
            .with_pull_voucher_strategy(PullVoucherStrategy::ClientVoucher)
            .with_payment_channel_signer(Arc::clone(&signer));
        let mut payload = payment_channel_payload(
            &session,
            solana_pubkey::Pubkey::new_unique(),
            solana_pubkey::Pubkey::new_unique(),
            44,
        );
        payload.mode = SessionMode::Pull;
        payload.transaction = None;

        let err = session
            .submit_server_payment_channel_open(&payload)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("payer must match operator signer"));
    }

    #[tokio::test]
    async fn lifecycle_runloop_operator_closes_idle_channel() {
        let session = Arc::new(test_session_mpp());
        session.start_lifecycle_runloop(Duration::from_millis(10));
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
        assert_eq!(session.committed_watermark(&opened.channel_id), Some(0));

        tokio::time::sleep(Duration::from_millis(60)).await;

        let voucher_header = handle.voucher_header(75).await.unwrap();
        let err = session.process(&voucher_header).await.unwrap_err();
        assert!(
            err.to_string().contains("close is pending"),
            "expected auto-close to reject later voucher, got: {err}"
        );
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
        assert_eq!(
            session.committed_watermark(&active.channel_id_str()),
            Some(60)
        );
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
    async fn run_pull_setup_rejects_when_chain_not_configured() {
        let session = test_session_mpp();
        let err = session
            .run_pull_setup(&no_tx_payload(CAP))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("operated-voucher pull sessions require a multi-delegate chain")
        );
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
    fn payment_channel_open_params_validates_stablecoin_symbols_via_sdk() {
        let session = SessionMpp::new(
            SessionConfig {
                currency: "USDC".to_string(),
                network: "localnet".to_string(),
                ..test_session_config()
            },
            "test-secret",
        );
        let payer = solana_pubkey::Pubkey::new_unique();
        let authorized_signer = solana_pubkey::Pubkey::new_unique();
        let payee = solana_pubkey::Pubkey::try_from(session.session_config.recipient.as_str())
            .expect("valid test payee");
        let mint = solana_pubkey::Pubkey::try_from(solana_mpp::mints::USDC_MAINNET)
            .expect("valid USDC mint");
        let program_id = session
            .session_config
            .program_id
            .unwrap_or_else(solana_mpp::program::payment_channels::default_program_id);
        let params = solana_mpp::program::payment_channels::OpenChannelParams {
            payer,
            payee,
            mint,
            authorized_signer,
            salt: 99,
            deposit: CAP,
            grace_period: 900,
            recipients: vec![],
            token_program: spl_token_program(),
            program_id,
        };
        let channel = solana_mpp::program::payment_channels::derive_channel_addresses(&params)
            .channel
            .to_string();
        let payload = OpenPayload::payment_channel(
            channel,
            CAP.to_string(),
            payer.to_string(),
            payee.to_string(),
            mint.to_string(),
            params.salt,
            params.grace_period,
            authorized_signer.to_string(),
            "pending".to_string(),
        );

        let parsed = session.payment_channel_open_params(&payload).unwrap();
        assert_eq!(parsed.mint, mint);

        let mut tampered = payload;
        tampered.mint = Some(solana_pubkey::Pubkey::new_unique().to_string());
        let err = session.payment_channel_open_params(&tampered).unwrap_err();
        assert!(err.to_string().contains("mint does not match"));
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
