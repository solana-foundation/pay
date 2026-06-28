//! Server-side session intent — channel lifecycle and voucher verification.
//!
//! Wraps [`pay_kit::mpp::server::session::SessionServer`] with an in-memory
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
use pay_kit::mpp::server::session::{FinalizeParams, SessionConfig, SessionServer};
use pay_kit::mpp::settlement::worker::{RpcBroadcaster, SettlementConfig, SettlementHandle, spawn};
use pay_kit::mpp::solana_keychain::SolanaSigner;
use pay_kit::mpp::store::{ChannelState, MemoryChannelStore};
use pay_kit::mpp::{
    Base64UrlJson, CommitReceipt, OpenPayload, PaymentChallenge, SessionAction, SessionMode,
    SessionPullVoucherStrategy, parse_authorization,
};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::{Error, Result};

const INTENT: &str = "session";
const METHOD: &str = "solana";
const DEFAULT_REALM: &str = "MPP Session";
fn session_close_already_requested(error: &pay_kit::mpp::Error) -> bool {
    error.to_string().contains("Close already requested")
}

fn session_close_already_finalized(error: &pay_kit::mpp::Error) -> bool {
    error.to_string().contains("already finalized")
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

#[derive(Clone)]
struct SessionOperatorRuntime {
    server: Arc<SessionServer<MemoryChannelStore>>,
    rpc_url: Option<String>,
    payment_channel_signer: Arc<Mutex<Option<Arc<dyn SolanaSigner>>>>,
    payment_channel_payer_signer: Arc<Mutex<Option<Arc<dyn SolanaSigner>>>>,
    committed_watermarks: Arc<Mutex<HashMap<String, u64>>>,
    /// Batched settlement worker, spawned lazily on first close (the signer is
    /// set after construction). Concurrent closes pack into shared txs.
    settlement_worker: Arc<tokio::sync::OnceCell<SettlementHandle>>,
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

    async fn operator_close_channel(&self, channel_id: &str) -> Result<SessionCloseResult> {
        use pay_kit::mpp::ClosePayload;

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

        use pay_kit::mpp::solana_rpc_client::nonblocking::rpc_client::RpcClient;
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
            pay_kit::mpp::program::payment_channels::build_settle_and_finalize_instructions(
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
                |split| pay_kit::mpp::program::payment_channels::Distribution {
                    recipient: split.recipient,
                    bps: split.bps,
                },
            )
            .collect::<Vec<_>>();
        instructions.push(
            pay_kit::mpp::program::payment_channels::build_distribute_instruction(
                &params.channel_id,
                &payer,
                // rentPayer is pinned to the operator (the settlement fee payer) —
                // the only tx signer able to fund any ATA creation.
                &signer.pubkey(),
                &params.recipient,
                &pay_kit::mpp::program::payment_channels::treasury_owner(),
                &mint,
                &recipients,
                &token_program,
                &params.program_id,
            ),
        );

        // Route through the shared batched worker: concurrent closes pack into
        // shared transactions, signed once by the operator and broadcast.
        let operator = signer.pubkey();
        let handle = self
            .settlement_worker
            .get_or_init(|| {
                let signer = Arc::clone(&signer);
                async move {
                    spawn(
                        SettlementConfig::new(operator, signer),
                        Arc::new(RpcBroadcaster::new(rpc_url)),
                    )
                }
            })
            .await;
        match handle
            .settle(params.channel_id.to_string(), instructions)
            .await
        {
            Ok(signature) => Ok(Some(signature)),
            Err(e) => Err(Error::Mpp(format!("payment-channel settlement: {e}"))),
        }
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
    ConfigureCloseDelay { close_delay: Option<Duration> },
    Touch { channel_id: String },
    Remove { channel_id: String },
}

struct SessionLifecycleRunloop {
    runtime: SessionOperatorRuntime,
    close_delay: Option<Duration>,
    rx: mpsc::UnboundedReceiver<SessionLifecycleCommand>,
    deadlines: HashMap<String, Instant>,
}

impl SessionLifecycleRunloop {
    fn new(
        runtime: SessionOperatorRuntime,
        rx: mpsc::UnboundedReceiver<SessionLifecycleCommand>,
    ) -> Self {
        Self {
            runtime,
            close_delay: None,
            rx,
            deadlines: HashMap::new(),
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
        self.deadlines.values().copied().min()
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
    challenge_binding_secret: String,
    realm: String,
    rpc_url: Option<String>,
    payment_channel_signer: Arc<Mutex<Option<Arc<dyn SolanaSigner>>>>,
    payment_channel_payer_signer: Arc<Mutex<Option<Arc<dyn SolanaSigner>>>>,
    committed_watermarks: Arc<Mutex<HashMap<String, u64>>>,
    pull_sessions: Arc<Mutex<HashSet<String>>>,
    lifecycle: SessionLifecycleHandle,
    operator_runtime: SessionOperatorRuntime,
    pull_voucher_strategy: PullVoucherStrategy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PullVoucherStrategy {
    #[default]
    Disabled,
    ClientVoucher,
}

impl SessionMpp {
    /// Network slug (for explorer/receipt URLs).
    pub fn network(&self) -> &str {
        &self.session_config.network
    }

    /// Create from a [`SessionConfig`] and an HMAC secret key.
    pub fn new(config: SessionConfig, challenge_binding_secret: impl Into<String>) -> Self {
        let session_config = config.clone();
        let server = Arc::new(SessionServer::new(config, MemoryChannelStore::new()));
        let payment_channel_signer = Arc::new(Mutex::new(None));
        let payment_channel_payer_signer = Arc::new(Mutex::new(None));
        let committed_watermarks = Arc::new(Mutex::new(HashMap::new()));
        let pull_sessions = Arc::new(Mutex::new(HashSet::new()));
        let operator_runtime = SessionOperatorRuntime {
            server: Arc::clone(&server),
            rpc_url: session_config.rpc_url.clone(),
            payment_channel_signer: Arc::clone(&payment_channel_signer),
            payment_channel_payer_signer: Arc::clone(&payment_channel_payer_signer),
            committed_watermarks: Arc::clone(&committed_watermarks),
            settlement_worker: Arc::new(tokio::sync::OnceCell::new()),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let runloop = SessionLifecycleRunloop::new(operator_runtime.clone(), rx);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(runloop.run());
        } else {
            tracing::debug!("session lifecycle runloop not started; no tokio runtime is active");
        }

        Self {
            rpc_url: session_config.rpc_url.clone(),
            server,
            session_config,
            challenge_binding_secret: challenge_binding_secret.into(),
            realm: DEFAULT_REALM.to_string(),
            payment_channel_signer,
            payment_channel_payer_signer,
            committed_watermarks,
            pull_sessions,
            lifecycle: SessionLifecycleHandle { tx },
            operator_runtime,
            pull_voucher_strategy: PullVoucherStrategy::Disabled,
        }
    }

    pub fn with_realm(mut self, realm: impl Into<String>) -> Self {
        self.realm = realm.into();
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
        }
        if request.modes == [SessionMode::Push] {
            request.modes.clear();
        }
        request.recent_blockhash = self.prefetch_latest_blockhash();
        let encoded = Base64UrlJson::from_typed(&request)
            .map_err(|e| Error::Mpp(format!("Failed to encode session request: {e}")))?;
        Ok(PaymentChallenge::with_challenge_binding_secret(
            &self.challenge_binding_secret,
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
    #[tracing::instrument(name = "session_process", skip_all)]
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
                self.touch_channel(state.channel_id.clone());
                Ok(SessionOutcome::Active(state))
            }

            SessionAction::Voucher(p) => {
                let cumulative = self
                    .server
                    .verify_voucher(p)
                    .await
                    .map_err(|e| Error::PaymentRejected(e.to_string()))?
                    .cumulative;
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
        request: pay_kit::mpp::server::session::DeliveryRequest,
    ) -> Result<pay_kit::mpp::MeteringDirective> {
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
                "token-account delegation pull sessions are no longer supported; use client-voucher payment channels"
                    .to_string(),
            ));
        }
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

        let instruction = pay_kit::mpp::program::payment_channels::build_open_instruction(&params);
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
    ) -> Result<pay_kit::mpp::program::payment_channels::OpenChannelParams> {
        self.server
            .payment_channel_open_params(payload)
            .map_err(|e| Error::Mpp(e.to_string()))
    }

    /// Best-effort prefetch of the latest blockhash for session challenges.
    ///
    /// Calls the same RPC as [`fetch_latest_blockhash`] but swallows errors
    /// (logged at debug) since the challenge remains valid without this field.
    fn prefetch_latest_blockhash(&self) -> Option<String> {
        use pay_kit::mpp::solana_rpc_client::rpc_client::RpcClient;

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
    use pay_kit::mpp::protocol::solana::programs;
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
    use pay_kit::mpp::solana_rpc_client::rpc_client::RpcClient;

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
        use pay_kit::mpp::solana_rpc_client::rpc_client::RpcClient;
        use solana_commitment_config::CommitmentConfig;

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
    rpc: &pay_kit::mpp::solana_rpc_client::rpc_client::RpcClient,
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
    use pay_kit::mpp::solana_keychain::{SolanaSigner, memory::MemorySigner};
    use pay_kit::mpp::{PaymentCredential, format_authorization};
    use std::sync::Arc;

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
        let challenge = PaymentChallenge::with_challenge_binding_secret(
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
            pay_kit::mpp::client::session::ActiveSession::new(channel_id, test_session_signer());

        let open_action = active.open_action(CAP, "open_sig");
        let open_header =
            pay_kit::mpp::format_authorization(&pay_kit::mpp::PaymentCredential::new(
                challenge.to_echo(),
                serde_json::to_value(open_action).unwrap(),
            ))
            .unwrap();
        let SessionOutcome::Active(_) = session.process(&open_header).await.unwrap() else {
            panic!("expected open outcome");
        };

        let directive = session
            .server
            .begin_delivery(pay_kit::mpp::server::session::DeliveryRequest::new(
                active.channel_id_str(),
                60,
            ))
            .await
            .unwrap();
        let voucher = active.prepare_increment(60).await.unwrap();
        let commit_action = SessionAction::Commit(pay_kit::mpp::CommitPayload {
            delivery_id: directive.delivery_id.clone(),
            voucher,
        });
        let commit_header =
            pay_kit::mpp::format_authorization(&pay_kit::mpp::PaymentCredential::new(
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
        let challenge = pay_kit::mpp::parse_www_authenticate(&header).unwrap();
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
            .unwrap_or_else(pay_kit::mpp::program::payment_channels::default_program_id);
        let token_program = spl_token_program();
        let params = pay_kit::mpp::program::payment_channels::OpenChannelParams {
            payer,
            rent_payer: payer,
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
        let channel = pay_kit::mpp::program::payment_channels::derive_channel_addresses(&params)
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
        let mint = solana_pubkey::Pubkey::try_from(pay_kit::mpp::mints::USDC_MAINNET)
            .expect("valid USDC mint");
        let program_id = session
            .session_config
            .program_id
            .unwrap_or_else(pay_kit::mpp::program::payment_channels::default_program_id);
        let params = pay_kit::mpp::program::payment_channels::OpenChannelParams {
            payer,
            rent_payer: payer,
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
        let channel = pay_kit::mpp::program::payment_channels::derive_channel_addresses(&params)
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
