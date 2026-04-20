//! Server-side session intent — channel lifecycle and voucher verification.
//!
//! Wraps [`solana_mpp::server::session::SessionServer`] with an in-memory
//! channel store and provides challenge issuance + action dispatch that fits
//! the pay-core middleware pattern.
//!
//! # Pull-mode session flow
//!
//! ```text
//! Client sends `open` with pre-signed txs (initDelegationTx, updateDelegationTx)
//!   │
//!   ▼
//! Server fetches MultiDelegate + FixedDelegation state from RPC
//!   │
//!   ├─ MultiDelegate missing    → submit initDelegationTx    (individual, not batched)
//!   ├─ Delegation cap too low   → submit updateDelegationTx  (individual, not batched)
//!   └─ Already sufficient       → skip
//!   │
//!   ▼
//! process_open() records channel state
//!   │
//!   ▼
//! Enqueue Fiber channel open in the configured batch processor interval
//! ```
//!
//! Multi-delegator accounts are **long-lived**: most returning clients take the
//! "already sufficient" path with zero on-chain overhead.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use solana_mpp::program::multi_delegator::{
    MultiDelegateOnChainState, MultiDelegateSetupAction, assess_multi_delegate_setup,
};
use solana_mpp::server::session::{FinalizeParams, SessionConfig, SessionServer};
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::store::{ChannelState, MemoryChannelStore};
use solana_mpp::{
    Base64UrlJson, OpenPayload, PaymentChallenge, SessionAction, SessionMode, parse_authorization,
};

use crate::{Error, Result};

const INTENT: &str = "session";
const METHOD: &str = "solana";
const DEFAULT_REALM: &str = "MPP Session";
const FIXED_DELEGATION_CAP_OFFSET: usize = 107;
const FIXED_DELEGATION_CAP_LEN: usize = 8;
const FIBER_CHANNEL_DATA_SIZE: u64 = 42;
const FIBER_CHANNEL_DEPOSIT_OFFSET: usize = 0;
const FIBER_CHANNEL_STATUS_OFFSET: usize = 40;
const FIBER_CHANNEL_STATUS_OPEN: u8 = 0;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FiberChannelAccountState {
    account_already_exists: bool,
    existing_deposit: Option<u64>,
    existing_status: Option<u8>,
    already_open: bool,
}

fn inspect_existing_fiber_channel(data: Option<&[u8]>) -> FiberChannelAccountState {
    let account_already_exists =
        data.is_some_and(|bytes| bytes.len() == FIBER_CHANNEL_DATA_SIZE as usize);
    let existing_deposit = data.and_then(|bytes| {
        let bytes = bytes.get(FIBER_CHANNEL_DEPOSIT_OFFSET..FIBER_CHANNEL_DEPOSIT_OFFSET + 8)?;
        let mut amount = [0u8; 8];
        amount.copy_from_slice(bytes);
        Some(u64::from_le_bytes(amount))
    });
    let existing_status = data.and_then(|bytes| bytes.get(FIBER_CHANNEL_STATUS_OFFSET).copied());
    let already_open = account_already_exists
        && existing_deposit.unwrap_or(0) > 0
        && existing_status == Some(FIBER_CHANNEL_STATUS_OPEN);

    FiberChannelAccountState {
        account_already_exists,
        existing_deposit,
        existing_status,
        already_open,
    }
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

// ── Pull-mode batch processor ──────────────────────────────────────────────
//
// Pull-mode session open flow:
//
//   1. Multi-delegator setup (per-session, NOT batched):
//      Each client's delegation setup requires a
//      dedicated transaction and must be confirmed before proceeding.
//
//   2. Fiber channel open (queued after delegation confirms, BATCHED):
//      Once the delegation is confirmed the operator opens a Fiber payment
//      channel on the client's behalf.  Multiple `InitChannel` instructions
//      can be packed into one Solana transaction, so the runloop accumulates
//      them and flushes on the configured interval.

/// A queued Fiber channel open waiting to be included in the next batch tx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelOpenItem {
    /// Client's wallet pubkey (base58) — the channel depositor.
    pub owner: String,
    /// Client's SPL token account (base58) — the funding source.
    pub token_account: String,
    /// Deposit amount for this channel (base units).
    pub deposit: u64,
    /// Fiber distribution hash committed for this channel.
    pub distribution_hash: [u8; 16],
}

/// Handle to the background Fiber-channel-open batch processor.
pub struct OpenChannelBatcher {
    tx: mpsc::UnboundedSender<ChannelOpenItem>,
}

type BatchSubmitFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type BatchSubmitter = Arc<dyn Fn(Vec<ChannelOpenItem>) -> BatchSubmitFuture + Send + Sync>;

impl OpenChannelBatcher {
    /// Queue a Fiber channel open for the next batch submission.
    pub(crate) fn enqueue(&self, item: ChannelOpenItem) {
        let _ = self.tx.send(item);
    }
}

/// Spawn the Fiber-channel-open batch runloop and return a [`OpenChannelBatcher`] handle.
///
/// On each configured tick the runloop drains pending [`ChannelOpenItem`]s and
/// submits a single transaction containing one Fiber `InitChannel` instruction
/// per item.
/// Must be called from within a running Tokio runtime.
pub fn spawn_open_channel_batcher(
    operator_signer: Arc<dyn SolanaSigner>,
    rpc_url: String,
    fiber_program_id: solana_pubkey::Pubkey,
    interval_ms: u64,
) -> OpenChannelBatcher {
    tracing::info!(
        interval_ms,
        operator = %operator_signer.pubkey(),
        fiber_program_id = %fiber_program_id,
        "configured Fiber channel-open batcher"
    );
    let submitter: BatchSubmitter = Arc::new(move |batch: Vec<ChannelOpenItem>| {
        let operator_signer = Arc::clone(&operator_signer);
        let rpc_url = rpc_url.clone();
        Box::pin(async move {
            submit_channel_opens(
                &batch,
                Arc::clone(&operator_signer),
                &rpc_url,
                fiber_program_id,
            )
            .await
        })
    });
    spawn_open_channel_batcher_with_submitter(submitter, interval_ms)
}

fn spawn_open_channel_batcher_with_submitter(
    submitter: BatchSubmitter,
    interval_ms: u64,
) -> OpenChannelBatcher {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(channel_open_batch_runloop(rx, submitter, interval_ms));
    OpenChannelBatcher { tx }
}

async fn channel_open_batch_runloop(
    mut rx: mpsc::UnboundedReceiver<ChannelOpenItem>,
    submitter: BatchSubmitter,
    interval_ms: u64,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;

    let mut pending: Vec<ChannelOpenItem> = Vec::new();

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if !pending.is_empty() {
                    let batch = std::mem::take(&mut pending);
                    tracing::info!(
                        count = batch.len(),
                        interval_ms,
                        owners = %batch.iter().map(|item| item.owner.as_str()).collect::<Vec<_>>().join(","),
                        "submitting Fiber channel-open batch"
                    );
                    if let Err(e) = submitter(batch.clone()).await {
                        tracing::error!(count = batch.len(), %e, "channel-open batch submission failed");
                    }
                }
            }
            item = rx.recv() => {
                match item {
                    Some(item) => {
                        let next_depth = pending.len() + 1;
                        tracing::info!(
                            owner = %item.owner,
                            token_account = %item.token_account,
                            deposit = item.deposit,
                            queue_depth = next_depth,
                            interval_ms,
                            "channel open queued"
                        );
                        pending.push(item);
                    }
                    None => {
                        // Sender dropped — flush and exit.
                        if !pending.is_empty() {
                            let batch = std::mem::take(&mut pending);
                            tracing::info!(
                                count = batch.len(),
                                owners = %batch.iter().map(|item| item.owner.as_str()).collect::<Vec<_>>().join(","),
                                "flushing channel-open batch on shutdown"
                            );
                            if let Err(e) = submitter(batch.clone()).await {
                                tracing::error!(count = batch.len(), %e, "channel-open batch flush failed");
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
}

async fn submit_channel_opens(
    batch: &[ChannelOpenItem],
    operator_signer: Arc<dyn SolanaSigner>,
    rpc_url: &str,
    fiber_program_id: solana_pubkey::Pubkey,
) -> Result<()> {
    use solana_instruction::{AccountMeta, Instruction};
    use solana_message::Message;
    use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
    use solana_system_interface::instruction as system_instruction;
    use solana_transaction::Transaction;

    const IX_OPEN: u8 = 0;

    if batch.is_empty() {
        return Ok(());
    }

    let rpc = RpcClient::new(rpc_url.to_string());
    let operator = operator_signer.pubkey();
    let rent_lamports = rpc
        .get_minimum_balance_for_rent_exemption(FIBER_CHANNEL_DATA_SIZE as usize)
        .map_err(|e| Error::Mpp(format!("failed to fetch Fiber rent exemption: {e}")))?;

    let mut instructions = Vec::with_capacity(batch.len() * 2);

    for item in batch {
        let seed = fiber_channel_seed(&item.owner, &item.token_account);
        let channel = solana_pubkey::Pubkey::create_with_seed(&operator, &seed, &fiber_program_id)
            .map_err(|e| Error::Mpp(format!("failed to derive Fiber channel address: {e}")))?;

        let existing_account = rpc.get_account(&channel).ok();
        let existing_state = inspect_existing_fiber_channel(
            existing_account
                .as_ref()
                .map(|account| account.data.as_slice()),
        );

        tracing::info!(
            owner = %item.owner,
            token_account = %item.token_account,
            %channel,
            seed,
            create_account = !existing_state.account_already_exists,
            already_open = existing_state.already_open,
            existing_owner = ?existing_account.as_ref().map(|account| account.owner.to_string()),
            existing_lamports = ?existing_account.as_ref().map(|account| account.lamports),
            existing_deposit = existing_state.existing_deposit,
            existing_status = existing_state.existing_status,
            "prepared Fiber channel-open item"
        );

        if existing_state.already_open {
            tracing::info!(
                owner = %item.owner,
                token_account = %item.token_account,
                %channel,
                "skipping Fiber open for already-open channel"
            );
            continue;
        }

        if !existing_state.account_already_exists {
            instructions.push(system_instruction::create_account_with_seed(
                &operator,
                &channel,
                &operator,
                &seed,
                rent_lamports,
                FIBER_CHANNEL_DATA_SIZE,
                &fiber_program_id,
            ));
        }

        let mut data = Vec::with_capacity(1 + 8 + 16);
        data.push(IX_OPEN);
        data.extend_from_slice(&item.deposit.to_le_bytes());
        data.extend_from_slice(&item.distribution_hash);

        instructions.push(Instruction {
            program_id: fiber_program_id,
            accounts: vec![
                AccountMeta::new_readonly(operator, true),
                AccountMeta::new(channel, false),
            ],
            data,
        });
    }

    if instructions.is_empty() {
        tracing::info!(
            count = batch.len(),
            "no Fiber channel-open instructions needed"
        );
        return Ok(());
    }

    let blockhash = rpc
        .get_latest_blockhash()
        .map_err(|e| Error::Mpp(format!("failed to fetch latest blockhash: {e}")))?;
    tracing::info!(
        count = batch.len(),
        instruction_count = instructions.len(),
        %blockhash,
        "signing Fiber channel-open transaction"
    );
    let message = Message::new_with_blockhash(&instructions, Some(&operator), &blockhash);
    let mut tx = Transaction::new_unsigned(message);
    operator_signer
        .sign_transaction(&mut tx)
        .await
        .map_err(|e| Error::Mpp(format!("failed to sign Fiber batch-open tx: {e}")))?;

    let signature = rpc
        .send_and_confirm_transaction(&tx)
        .map_err(|e| Error::Mpp(format!("Fiber channel-open batch submission failed: {e}")))?;

    tracing::info!(count = batch.len(), %signature, "Fiber channel-open batch confirmed");
    Ok(())
}

fn fiber_channel_seed(owner: &str, token_account: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(owner.as_bytes());
    hasher.update(b":");
    hasher.update(token_account.as_bytes());
    let hash = hasher.finalize();
    let mut seed = String::with_capacity(32);
    for byte in &hash.as_bytes()[..16] {
        use std::fmt::Write as _;
        let _ = write!(&mut seed, "{byte:02x}");
    }
    seed
}

// ── Session outcome ────────────────────────────────────────────────────────

/// The result of processing a session action.
pub enum SessionOutcome {
    /// `open` or `topup` — channel state after the action.
    Active(ChannelState),
    /// `voucher` accepted — the new settled cumulative (base units).
    Voucher(u64),
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
/// Pull-mode sessions go through a two-step on-chain process:
/// 1. Multi-delegator setup (individual, confirmed via [`MultiDelegateChain`])
/// 2. Fiber channel open (batched via [`OpenChannelBatcher`])
pub struct SessionMpp {
    server: SessionServer<MemoryChannelStore>,
    secret_key: String,
    realm: String,
    distribution_hash: [u8; 16],
    rpc_url: Option<String>,
    /// Interface to on-chain multi-delegate state (optional; pull-mode setup
    /// is skipped when absent).
    multi_delegate_chain: Option<Box<dyn MultiDelegateChain>>,
    /// Background batch processor for Fiber channel opens.
    open_channel_batcher: Option<OpenChannelBatcher>,
}

impl SessionMpp {
    /// Create from a [`SessionConfig`] and an HMAC secret key.
    pub fn new(config: SessionConfig, secret_key: impl Into<String>) -> Self {
        let recipient =
            solana_pubkey::Pubkey::try_from(config.recipient.as_str()).unwrap_or_default();
        let splits = config
            .splits
            .iter()
            .map(|split| (split.recipient, split.amount))
            .collect::<Vec<_>>();
        Self {
            distribution_hash: solana_mpp::server::session::compute_distribution_hash(
                &recipient, &splits,
            ),
            rpc_url: config.rpc_url.clone(),
            server: SessionServer::new(config, MemoryChannelStore::new()),
            secret_key: secret_key.into(),
            realm: DEFAULT_REALM.to_string(),
            multi_delegate_chain: None,
            open_channel_batcher: None,
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

    /// Enable the Fiber channel-open batch processor for pull-mode sessions.
    ///
    /// Spawns a Tokio task that collects opens and submits a batched
    /// transaction on the configured interval. Must be called from within a
    /// Tokio runtime.
    pub fn with_open_channel_batcher(
        mut self,
        operator_signer: Arc<dyn SolanaSigner>,
        rpc_url: impl Into<String>,
        fiber_program_id: solana_pubkey::Pubkey,
        interval_ms: u64,
    ) -> Self {
        self.open_channel_batcher = Some(spawn_open_channel_batcher(
            operator_signer,
            rpc_url.into(),
            fiber_program_id,
            interval_ms,
        ));
        self
    }

    #[doc(hidden)]
    pub fn with_test_open_channel_batcher<F, Fut>(self, submitter: F) -> Self
    where
        F: Fn(Vec<(String, String, u64)>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.with_test_open_channel_batcher_interval(400, submitter)
    }

    #[doc(hidden)]
    pub fn with_test_open_channel_batcher_interval<F, Fut>(
        mut self,
        interval_ms: u64,
        submitter: F,
    ) -> Self
    where
        F: Fn(Vec<(String, String, u64)>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let submitter = Arc::new(submitter);
        let adapter: BatchSubmitter = Arc::new(move |batch: Vec<ChannelOpenItem>| {
            let submitter = Arc::clone(&submitter);
            let mapped = batch
                .into_iter()
                .map(|item| (item.owner, item.token_account, item.deposit))
                .collect();
            Box::pin(async move { submitter(mapped).await })
        });
        self.open_channel_batcher = Some(spawn_open_channel_batcher_with_submitter(
            adapter,
            interval_ms,
        ));
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
    /// For pull-mode `open` actions:
    /// 1. Runs the multi-delegator pre-flight check (if a chain is configured).
    /// 2. Calls [`SessionServer::process_open`] to persist channel state.
    /// 3. Enqueues the Fiber channel open in the batch processor.
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

                let state = self
                    .server
                    .process_open(p)
                    .await
                    .map_err(|e| Error::Mpp(format!("Session open failed: {e}")))?;

                if p.mode == SessionMode::Pull {
                    self.enqueue_channel_open(p, &state);
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

    /// Enqueue a Fiber channel open in the batch processor (if configured).
    fn enqueue_channel_open(&self, payload: &OpenPayload, state: &ChannelState) {
        if let Some(batcher) = &self.open_channel_batcher
            && let (Some(token_account), Some(owner)) =
                (payload.token_account.as_deref(), payload.owner.as_deref())
        {
            batcher.enqueue(ChannelOpenItem {
                owner: owner.to_string(),
                token_account: token_account.to_string(),
                deposit: state.deposit,
                distribution_hash: self.distribution_hash,
            });
            tracing::info!(owner, deposit = state.deposit, "Fiber channel open queued");
        }
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

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::session::SessionHandle;
    use solana_mpp::program::multi_delegator::MultiDelegateOnChainState;
    use solana_mpp::solana_keychain::{SolanaSigner, memory::MemorySigner};
    use solana_mpp::{PaymentCredential, format_authorization};
    use std::sync::{Arc, Mutex};
    use tokio::time::{sleep, timeout};

    type BatchLog = Arc<Mutex<Vec<Vec<(String, String, u64)>>>>;

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
    fn inspect_existing_fiber_channel_detects_open_channel() {
        let mut data = vec![0u8; FIBER_CHANNEL_DATA_SIZE as usize];
        data[FIBER_CHANNEL_DEPOSIT_OFFSET..FIBER_CHANNEL_DEPOSIT_OFFSET + 8]
            .copy_from_slice(&123u64.to_le_bytes());
        data[FIBER_CHANNEL_STATUS_OFFSET] = FIBER_CHANNEL_STATUS_OPEN;

        assert_eq!(
            inspect_existing_fiber_channel(Some(&data)),
            FiberChannelAccountState {
                account_already_exists: true,
                existing_deposit: Some(123),
                existing_status: Some(FIBER_CHANNEL_STATUS_OPEN),
                already_open: true,
            }
        );
    }

    #[test]
    fn inspect_existing_fiber_channel_rejects_short_or_closed_accounts() {
        let short = vec![0u8; 4];
        assert_eq!(
            inspect_existing_fiber_channel(Some(&short)),
            FiberChannelAccountState {
                account_already_exists: false,
                existing_deposit: None,
                existing_status: None,
                already_open: false,
            }
        );

        let mut closed = vec![0u8; FIBER_CHANNEL_DATA_SIZE as usize];
        closed[FIBER_CHANNEL_DEPOSIT_OFFSET..FIBER_CHANNEL_DEPOSIT_OFFSET + 8]
            .copy_from_slice(&123u64.to_le_bytes());
        closed[FIBER_CHANNEL_STATUS_OFFSET] = 9;
        let state = inspect_existing_fiber_channel(Some(&closed));
        assert!(state.account_already_exists);
        assert_eq!(state.existing_deposit, Some(123));
        assert_eq!(state.existing_status, Some(9));
        assert!(!state.already_open);
    }

    #[test]
    fn fiber_channel_seed_is_deterministic_and_input_sensitive() {
        let first = fiber_channel_seed("owner-a", "token-a");
        let second = fiber_channel_seed("owner-a", "token-a");
        let different = fiber_channel_seed("owner-a", "token-b");
        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        assert_ne!(first, different);
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

    #[tokio::test]
    async fn enqueue_channel_open_requires_batcher_and_fields() {
        let session = test_session_mpp();
        let state = ChannelState {
            channel_id: "chan-1".to_string(),
            authorized_signer: "auth-1".to_string(),
            deposit: CAP,
            cumulative: 0,
            finalized: false,
            highest_voucher_signature: None,
            close_requested_at: None,
            operator: Some("walletABC".to_string()),
        };
        session.enqueue_channel_open(&no_tx_payload(CAP), &state);

        let submitted: BatchLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&submitted);
        let session =
            test_session_mpp().with_test_open_channel_batcher_interval(20, move |batch| {
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock().unwrap().push(batch);
                    Ok(())
                }
            });
        let mut payload = no_tx_payload(CAP);
        payload.token_account = None;
        session.enqueue_channel_open(&payload, &state);

        sleep(Duration::from_millis(40)).await;
        assert!(submitted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn batcher_flushes_on_interval() {
        let submitted: BatchLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&submitted);
        let session =
            test_session_mpp().with_test_open_channel_batcher_interval(20, move |batch| {
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock().unwrap().push(batch);
                    Ok(())
                }
            });
        let state = ChannelState {
            channel_id: "chan-interval".to_string(),
            authorized_signer: "auth-interval".to_string(),
            deposit: CAP,
            cumulative: 0,
            finalized: false,
            highest_voucher_signature: None,
            close_requested_at: None,
            operator: Some("walletABC".to_string()),
        };

        session.enqueue_channel_open(&no_tx_payload(CAP), &state);

        timeout(Duration::from_secs(1), async {
            loop {
                if !submitted.lock().unwrap().is_empty() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("batch flush on interval");

        let batches = submitted.lock().unwrap().clone();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0],
            vec![("walletABC".to_string(), "tokacct111".to_string(), CAP)]
        );
    }

    #[tokio::test]
    async fn batcher_continues_after_submit_error() {
        let attempts = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&attempts);
        let batcher = spawn_open_channel_batcher_with_submitter(
            Arc::new(move |_batch: Vec<ChannelOpenItem>| {
                let sink = Arc::clone(&sink);
                Box::pin(async move {
                    *sink.lock().unwrap() += 1;
                    Err(Error::Mpp("simulated batch failure".to_string()))
                })
            }),
            20,
        );

        batcher.enqueue(ChannelOpenItem {
            owner: "owner-1".to_string(),
            token_account: "token-1".to_string(),
            deposit: 11,
            distribution_hash: [1; 16],
        });

        timeout(Duration::from_secs(1), async {
            loop {
                if *attempts.lock().unwrap() >= 1 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first batch attempt should happen");

        batcher.enqueue(ChannelOpenItem {
            owner: "owner-2".to_string(),
            token_account: "token-2".to_string(),
            deposit: 22,
            distribution_hash: [2; 16],
        });

        timeout(Duration::from_secs(1), async {
            loop {
                if *attempts.lock().unwrap() >= 2 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("batcher should accept later batches after an error");
    }

    #[tokio::test]
    async fn spawn_open_channel_batcher_can_start_and_stop_without_work() {
        use ed25519_dalek::SigningKey;

        let sk = SigningKey::generate(&mut rand::thread_rng());
        let vk = sk.verifying_key();
        let mut kp = [0u8; 64];
        kp[..32].copy_from_slice(sk.as_bytes());
        kp[32..].copy_from_slice(vk.as_bytes());
        let signer: Arc<dyn SolanaSigner> = Arc::new(MemorySigner::from_bytes(&kp).unwrap());
        let batcher = spawn_open_channel_batcher(
            signer,
            "http://127.0.0.1:8899".to_string(),
            solana_pubkey::Pubkey::new_unique(),
            50,
        );
        drop(batcher);
        sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn batcher_flushes_pending_items_on_shutdown() {
        let submitted: BatchLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&submitted);
        let submitter: BatchSubmitter = Arc::new(move |batch: Vec<ChannelOpenItem>| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                sink.lock().unwrap().push(
                    batch
                        .into_iter()
                        .map(|item| (item.owner, item.token_account, item.deposit))
                        .collect(),
                );
                Ok(())
            })
        });

        let batcher = spawn_open_channel_batcher_with_submitter(submitter, 10_000);
        batcher.enqueue(ChannelOpenItem {
            owner: "owner-1".to_string(),
            token_account: "token-1".to_string(),
            deposit: 11,
            distribution_hash: [1; 16],
        });
        batcher.enqueue(ChannelOpenItem {
            owner: "owner-2".to_string(),
            token_account: "token-2".to_string(),
            deposit: 22,
            distribution_hash: [2; 16],
        });

        drop(batcher);

        timeout(Duration::from_secs(1), async {
            loop {
                if submitted.lock().unwrap().len() == 1 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("batch flush on shutdown");

        let batches = submitted.lock().unwrap().clone();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0],
            vec![
                ("owner-1".to_string(), "token-1".to_string(), 11),
                ("owner-2".to_string(), "token-2".to_string(), 22),
            ]
        );
    }
}
