//! Reconcile durable MPP sessions and x402 batch-settlement channels.
//!
//! Gateways persist each accepted cumulative voucher in scheme-specific Redis
//! namespaces. This worker pushes session watermarks and idle closes, while
//! delegating batch claim, payout, forced-close finalization, and rent reclaim
//! to pay-kit's scheme implementation.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::routing::get;
use futures::{StreamExt, stream};
use pay_api_core::rpc::RpcClient;
use pay_kit::core::payment_channels;
use pay_kit::core::settlement::worker::{RpcBroadcaster, SettlementConfig, spawn};
use pay_kit::core::store::{
    ChannelState, ChannelStore, DEFAULT_FINALIZED_CHANNEL_RETENTION, RedisChannelStore, StoreError,
};
use pay_kit::core::tx_pipeline::{TxPipeline, TxPipelineConfig, TxPipelineError};
use pay_kit::mpp::server::session::{
    SessionConfig, SessionConfigSnapshot, SessionServer, channel_binding, session_open_is_terminal,
};
use pay_kit::mpp::solana_keychain::TransactionSigner;
use pay_worker::channel::{self, STATUS_CLOSING, STATUS_DISTRIBUTED, STATUS_OPEN, STATUS_SEALED};
use pay_worker::config::Config;
use pay_worker::error::JobError;
use pay_worker::resource_cleanup::ResourceCleaner;
use pay_worker::signer::build_fee_payer_signer;
use pay_worker::telemetry::{self, SettleSessionsMetrics};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const DEFAULT_REDIS_PREFIX: &str = "pay:session:v1:";
const DEFAULT_BATCH_REDIS_PREFIX: &str = "pay:batch:v1:";
const SESSION_SETTLEMENT_LOCK_KEY: &str = "pay:jobs:settle-sessions:lock";
const BATCH_SETTLEMENT_LOCK_KEY: &str = "pay:jobs:settle-batch:lock";
const DEFAULT_RECONCILIATION_INTERVAL_SECONDS: u64 = 10;
const DEFAULT_X402_SETTLEMENT_MAX_IDLE_SECONDS: u64 = 300;
const DEFAULT_X402_SNAPSHOT_MAX_AGE_SECONDS: u64 = 30;
const DEFAULT_X402_RECONCILIATION_CONCURRENCY: u64 = 64;
const DEFAULT_PORT: u64 = 8080;
const RESOURCE_CLEANUP_COMPLETED_AT: &str = "pay_resource_cleanup_completed_at";
const RESOURCE_CLEANUP_LAST_AT: &str = "pay_resource_cleanup_last_at";
const RESOURCE_CLEANUP_EMPTY_SINCE: &str = "pay_resource_cleanup_empty_since";
const DEFAULT_RESOURCE_CLEANUP_INTERVAL_SECONDS: u64 = 60;
const DEFAULT_RESOURCE_CLEANUP_QUIET_SECONDS: u64 = 7200;

struct LeaseHeartbeat {
    cancel: CancellationToken,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl LeaseHeartbeat {
    fn start_bound_close(
        store: RedisChannelStore,
        channel_id: String,
        owner: String,
        ttl: Duration,
    ) -> (Self, CancellationToken) {
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let lost = CancellationToken::new();
        let task_lost = lost.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval((ttl / 3).max(Duration::from_millis(1)));
            loop {
                tokio::select! {
                    () = task_cancel.cancelled() => return,
                    _ = interval.tick() => {
                        if let Err(error) = claim_due_close(
                            &store, &channel_id, unix_now_millis(), unix_now() as u64, &owner, ttl,
                        ).await {
                            task_lost.cancel();
                            warn!(%channel_id, %error, "bound close ownership lost; stopping renewal");
                            return;
                        }
                    }
                }
            }
        });
        (
            Self {
                cancel,
                handle: Some(handle),
            },
            lost,
        )
    }

    fn start(
        mut connection: redis::aio::ConnectionManager,
        lock_key: String,
        owner: String,
        ttl_seconds: u64,
    ) -> Self {
        const RENEW: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('EXPIRE', KEYS[1], ARGV[2])
end
return 0
"#;
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let renewal_interval = Duration::from_secs((ttl_seconds / 3).max(1));
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(renewal_interval);
            loop {
                tokio::select! {
                    () = task_cancel.cancelled() => return,
                    _ = interval.tick() => {
                        match redis::Script::new(RENEW)
                            .key(&lock_key)
                            .arg(&owner)
                            .arg(ttl_seconds)
                            .invoke_async::<i32>(&mut connection)
                            .await
                        {
                            Ok(1) => {}
                            Ok(_) => {
                                warn!("settlement lease ownership was lost; stopping renewal");
                                return;
                            }
                            Err(error) => {
                                warn!(%error, "failed to renew settlement lease");
                            }
                        }
                    }
                }
            }
        });
        Self {
            cancel,
            handle: Some(handle),
        }
    }

    async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take()
            && let Err(error) = handle.await
        {
            warn!(%error, "settlement lease heartbeat task failed");
        }
    }
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        // An error after lease acquisition must not detach a task that renews
        // the lock forever. The Redis TTL then provides the crash fallback.
        self.cancel.cancel();
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let _telemetry = telemetry::init("pay-jobs-settle-sessions");
    let run_once = match parse_bool_env("RUN_ONCE", true) {
        Ok(run_once) => run_once,
        Err(error) => return record_startup_failure(error),
    };
    let runtime = match SettlementRuntime::load().await {
        Ok(runtime) => runtime,
        Err(error) => return record_startup_failure(error),
    };
    if run_once {
        return record_iteration(run(&runtime).await, true);
    }

    let interval_seconds = match parse_u64_env(
        "SETTLEMENT_INTERVAL_SECONDS",
        DEFAULT_RECONCILIATION_INTERVAL_SECONDS,
    ) {
        Ok(0) => {
            return record_startup_failure(JobError::Config(
                "SETTLEMENT_INTERVAL_SECONDS must be greater than zero".into(),
            ));
        }
        Ok(interval_seconds) => interval_seconds,
        Err(error) => return record_startup_failure(error),
    };
    let port = match parse_u64_env("PORT", DEFAULT_PORT) {
        Ok(port) if u16::try_from(port).is_ok() => port as u16,
        Ok(_) => {
            return record_startup_failure(JobError::Config(
                "PORT must be between 0 and 65535".into(),
            ));
        }
        Err(error) => return record_startup_failure(error),
    };

    let address = format!("0.0.0.0:{port}");
    let listener = match tokio::net::TcpListener::bind(&address).await {
        Ok(listener) => listener,
        Err(error) => {
            return record_startup_failure(JobError::Config(format!(
                "failed to bind worker health endpoint on {address}: {error}"
            )));
        }
    };
    let app = Router::new().route("/health", get(|| async { "ok" }));
    info!(
        %address,
        interval_seconds,
        "continuous settle-sessions worker starting"
    );

    let cancel = CancellationToken::new();
    let server_shutdown = cancel.clone();
    let cancel_on_server_exit = cancel.clone();
    let server = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(server_shutdown.cancelled_owned())
            .await;
        cancel_on_server_exit.cancel();
        result
    });
    let cancel_on_signal = cancel.clone();
    let signal = tokio::spawn(async move {
        shutdown_signal().await;
        info!("settle-sessions shutdown signal received");
        cancel_on_signal.cancel();
    });

    run_continuously(
        &runtime,
        Duration::from_secs(interval_seconds),
        cancel.clone(),
    )
    .await;
    cancel.cancel();

    let server_result = server.await;
    if !signal.is_finished() {
        signal.abort();
    }
    let _ = signal.await;

    match server_result {
        Ok(Ok(())) => std::process::ExitCode::SUCCESS,
        Ok(Err(error)) => {
            error!(%error, "settle-sessions health server failed");
            std::process::ExitCode::FAILURE
        }
        Err(error) => {
            error!(%error, "settle-sessions health server task failed");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_continuously(
    runtime: &SettlementRuntime,
    interval: Duration,
    cancel: CancellationToken,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }
        let _ = record_iteration(run(runtime).await, false);
        if cancel.is_cancelled() {
            break;
        }
    }
}

fn record_iteration(
    result: Result<Vec<SettleSessionsMetrics>, JobError>,
    terminal: bool,
) -> std::process::ExitCode {
    match result {
        Ok(all_metrics) => {
            let failed = all_metrics.iter().any(|metrics| metrics.failures > 0);
            for metrics in &all_metrics {
                log_summary(metrics);
                telemetry::record_settle_sessions(metrics);
            }
            if !failed {
                info!(
                    event = if terminal {
                        "settle_sessions_exit"
                    } else {
                        "settle_sessions_iteration"
                    },
                    "payment-channel reconciliation finished"
                );
                std::process::ExitCode::SUCCESS
            } else {
                let failures: usize = all_metrics.iter().map(|metrics| metrics.failures).sum();
                error!(
                    event = if terminal {
                        "settle_sessions_exit"
                    } else {
                        "settle_sessions_iteration"
                    },
                    failures, "payment-channel reconciliation finished with failures"
                );
                std::process::ExitCode::FAILURE
            }
        }
        Err(error) => {
            let metrics = SettleSessionsMetrics {
                scheme: "all",
                outcome: "aborted",
                failures: 1,
                ..SettleSessionsMetrics::default()
            };
            telemetry::record_settle_sessions(&metrics);
            error!(
                event = if terminal {
                    "settle_sessions_exit"
                } else {
                    "settle_sessions_iteration"
                },
                outcome = "aborted",
                %error,
                "settle-sessions reconciliation aborted; will retry"
            );
            std::process::ExitCode::FAILURE
        }
    }
}

fn record_startup_failure(error: JobError) -> std::process::ExitCode {
    let metrics = SettleSessionsMetrics {
        scheme: "all",
        outcome: "aborted",
        failures: 1,
        ..SettleSessionsMetrics::default()
    };
    telemetry::record_settle_sessions(&metrics);
    error!(
        event = "settle_sessions_exit",
        outcome = "aborted",
        %error,
        "settle-sessions startup failed"
    );
    std::process::ExitCode::FAILURE
}

async fn run(runtime: &SettlementRuntime) -> Result<Vec<SettleSessionsMetrics>, JobError> {
    let mut metrics = Vec::with_capacity(2);
    if runtime.session.is_some() {
        metrics.push(match run_session(runtime).await {
            Ok(metrics) => metrics,
            Err(error) => {
                error!(scheme = "mpp/session", %error, "session reconciliation aborted");
                SettleSessionsMetrics {
                    scheme: "mpp/session",
                    outcome: "aborted",
                    failures: 1,
                    ..SettleSessionsMetrics::default()
                }
            }
        });
    }
    if runtime.batch.is_some() {
        metrics.push(match run_batch(runtime).await {
            Ok(metrics) => metrics,
            Err(error) => {
                error!(scheme = "x402/batch", %error, "batch reconciliation aborted");
                SettleSessionsMetrics {
                    scheme: "x402/batch",
                    outcome: "aborted",
                    failures: 1,
                    ..SettleSessionsMetrics::default()
                }
            }
        });
    }
    Ok(metrics)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

struct SettlementRuntime {
    session: Option<ChannelRuntime>,
    batch: Option<BatchRuntime>,
    dry_run: bool,
    lock_ttl: u64,
    network: String,
    rpc_url: String,
    treasury_owner: Pubkey,
    rpc: RpcClient,
    signer: Arc<dyn TransactionSigner>,
    operator: Pubkey,
    confirm_timeout: Duration,
    resource_cleaner: ResourceCleaner,
    resource_cleanup_interval: Duration,
    resource_cleanup_quiet: Duration,
}

struct ChannelRuntime {
    redis_url: String,
    store: RedisChannelStore,
}

struct BatchRuntime {
    redis_url: String,
    store: RedisChannelStore,
    settle_active_channels: bool,
    distribution_threshold_base_units: Option<u64>,
    settlement_max_idle: Duration,
    snapshot_max_age: Duration,
    reconciliation_concurrency: usize,
}

impl SettlementRuntime {
    async fn load() -> Result<Self, JobError> {
        let session_redis_url =
            optional_env("PAY_MPP_REDIS_URL").or_else(|| optional_env("PAY_SESSION_REDIS_URL"));
        let batch_redis_url = optional_env("PAY_X402_REDIS_URL");
        if session_redis_url.is_none() && batch_redis_url.is_none() {
            return Err(JobError::Config(
                "PAY_MPP_REDIS_URL, PAY_SESSION_REDIS_URL, or PAY_X402_REDIS_URL is required"
                    .into(),
            ));
        }
        let finalized_retention_seconds = match optional_env("PAY_MPP_FINALIZED_RETENTION_SECONDS")
        {
            Some(value) => value.parse().map_err(|_| {
                JobError::Config("PAY_MPP_FINALIZED_RETENTION_SECONDS must be an integer".into())
            })?,
            None => parse_u64_env(
                "PAY_SESSION_FINALIZED_RETENTION_SECONDS",
                DEFAULT_FINALIZED_CHANNEL_RETENTION.as_secs(),
            )?,
        };
        let finalized_retention = Duration::from_secs(finalized_retention_seconds);
        let dry_run = parse_bool_env("DRY_RUN", true)?;
        let lock_ttl = parse_u64_env("SETTLEMENT_LOCK_TTL_SECONDS", 300)?;
        let network = std::env::var("NETWORK")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "mainnet".to_string());
        let config = Config::load(&network)?;
        let rpc_url = config.rpc_url_for(&network)?.to_string();
        let treasury_owner = if config.treasury_owner.trim().is_empty() {
            payment_channels::treasury_owner_for_cluster(&network)
        } else {
            Pubkey::from_str(config.treasury_owner.trim()).map_err(|_| {
                JobError::Config(format!("invalid treasury_owner: {}", config.treasury_owner))
            })?
        };
        let rpc = RpcClient::new(Duration::from_millis(config.rpc_timeout_ms))?;
        let signer = build_fee_payer_signer(&config.send.fee_payer).await?;
        let operator = signer.pubkey();
        let confirm_timeout = Duration::from_secs(config.confirm_timeout_seconds);
        let resource_cleaner = ResourceCleaner::from_env().await?;
        let resource_cleanup_interval = Duration::from_secs(require_positive_u64(
            "PAY_RESOURCE_CLEANUP_INTERVAL_SECONDS",
            parse_u64_env(
                "PAY_RESOURCE_CLEANUP_INTERVAL_SECONDS",
                DEFAULT_RESOURCE_CLEANUP_INTERVAL_SECONDS,
            )?,
        )?);
        let resource_cleanup_quiet = Duration::from_secs(require_positive_u64(
            "PAY_RESOURCE_CLEANUP_QUIET_SECONDS",
            parse_u64_env(
                "PAY_RESOURCE_CLEANUP_QUIET_SECONDS",
                DEFAULT_RESOURCE_CLEANUP_QUIET_SECONDS,
            )?,
        )?);

        let session = if let Some(redis_url) = session_redis_url.as_ref() {
            let redis_prefix = optional_env("PAY_MPP_REDIS_PREFIX")
                .or_else(|| optional_env("PAY_SESSION_REDIS_PREFIX"))
                .unwrap_or_else(|| DEFAULT_REDIS_PREFIX.to_string());
            let store = RedisChannelStore::connect_with_finalized_retention(
                redis_url,
                redis_prefix,
                finalized_retention,
            )
            .await
            .map_err(|error| JobError::Config(format!("session Redis: {error}")))?;
            Some(ChannelRuntime {
                redis_url: redis_url.clone(),
                store,
            })
        } else {
            None
        };

        let batch = {
            let redis_url = batch_redis_url
                .clone()
                .or_else(|| session_redis_url.clone())
                .ok_or_else(|| JobError::Config("batch Redis URL is missing".into()))?;
            let redis_prefix = optional_env("PAY_X402_REDIS_PREFIX")
                .unwrap_or_else(|| DEFAULT_BATCH_REDIS_PREFIX.to_string());
            let store = RedisChannelStore::connect_with_finalized_retention(
                &redis_url,
                redis_prefix,
                finalized_retention,
            )
            .await
            .map_err(|error| JobError::Config(format!("batch Redis: {error}")))?;
            let settle_active_channels = parse_bool_env("PAY_X402_SETTLE_ACTIVE_CHANNELS", false)?;
            let distribution_threshold_base_units =
                parse_optional_positive_u64_env("PAY_X402_DISTRIBUTION_THRESHOLD_BASE_UNITS")?;
            let settlement_max_idle = Duration::from_secs(parse_u64_env(
                "PAY_X402_SETTLEMENT_MAX_IDLE_SECONDS",
                DEFAULT_X402_SETTLEMENT_MAX_IDLE_SECONDS,
            )?);
            let snapshot_max_age_seconds = require_positive_u64(
                "PAY_X402_SNAPSHOT_MAX_AGE_SECS",
                parse_u64_env(
                    "PAY_X402_SNAPSHOT_MAX_AGE_SECS",
                    DEFAULT_X402_SNAPSHOT_MAX_AGE_SECONDS,
                )?,
            )?;
            let snapshot_max_age = Duration::from_secs(snapshot_max_age_seconds);
            let reconciliation_concurrency = parse_u64_env(
                "PAY_X402_RECONCILIATION_CONCURRENCY",
                DEFAULT_X402_RECONCILIATION_CONCURRENCY,
            )?;
            let reconciliation_concurrency = usize::try_from(reconciliation_concurrency)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    JobError::Config(
                        "PAY_X402_RECONCILIATION_CONCURRENCY must be greater than zero".into(),
                    )
                })?;
            Some(BatchRuntime {
                redis_url,
                store,
                settle_active_channels,
                distribution_threshold_base_units,
                settlement_max_idle,
                snapshot_max_age,
                reconciliation_concurrency,
            })
        };

        Ok(Self {
            session,
            batch,
            dry_run,
            lock_ttl,
            network,
            rpc_url,
            treasury_owner,
            rpc,
            signer,
            operator,
            confirm_timeout,
            resource_cleaner,
            resource_cleanup_interval,
            resource_cleanup_quiet,
        })
    }
}

fn binding_error(error: impl std::fmt::Display) -> JobError {
    JobError::Config(format!("deployment session binding: {error}"))
}

async fn restore_bound_session(
    runtime: &SettlementRuntime,
    session: &ChannelRuntime,
    mut state: ChannelState,
) -> Result<(ChannelState, Option<SessionConfig>), JobError> {
    let Some(value) = channel_binding(&state).map_err(binding_error)? else {
        return Ok((state, None));
    };
    let binding = pay_types::deployment_policy::DeploymentSessionBinding::from_value(value.clone())
        .map_err(binding_error)?;
    let snapshot = SessionConfigSnapshot::from_channel(&state)
        .map_err(binding_error)?
        .ok_or_else(|| binding_error("missing configuration snapshot"))?;
    let mut config = SessionConfig {
        rpc_url: Some(runtime.rpc_url.clone()),
        fee_payer_signer: snapshot
            .requires_fee_payer()
            .then(|| runtime.signer.clone()),
        ..SessionConfig::default()
    };
    snapshot.apply_to(&mut config).map_err(binding_error)?;
    validate_bound_config(&binding, &config, &runtime.network, &runtime.operator)?;
    let server = SessionServer::new(config.clone(), Arc::new(session.store.clone()))
        .with_channel_binding(value);
    if resume_scanned_pending_open(&state, runtime.dry_run, |id| {
        let server = &server;
        async move {
            server
                .resume_pending_open(&id)
                .await
                .map_err(binding_error)?;
            Ok(())
        }
    })
    .await?
    {
        state = session
            .store
            .get_channel(&state.channel_id)
            .await
            .map_err(binding_error)?
            .ok_or_else(|| binding_error("recovered channel disappeared"))?;
    }
    server
        .require_channel_binding(&state)
        .map_err(binding_error)?;
    Ok((state, Some(config)))
}

/// Recovery is driven by durable enumeration, not by another opening request.
/// The callback is the kit verifier; keeping selection separate makes it
/// testable without broadcasting a funding transaction.
async fn resume_scanned_pending_open<F, Fut>(
    state: &ChannelState,
    dry_run: bool,
    resume: F,
) -> Result<bool, JobError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), JobError>>,
{
    if !state
        .pending_setup
        .as_ref()
        .is_some_and(|setup| setup.opens_channel)
    {
        return Ok(false);
    }
    if dry_run {
        return Err(binding_error(
            "pending open requires recovery; dry run leaves it untouched",
        ));
    }
    resume(state.channel_id.clone()).await?;
    Ok(true)
}

fn validate_bound_config(
    binding: &pay_types::deployment_policy::DeploymentSessionBinding,
    config: &SessionConfig,
    network: &str,
    operator: &Pubkey,
) -> Result<(), JobError> {
    let payout = binding
        .effective_payout(&config.operator)
        .map_err(binding_error)?;
    if config.network != network
        || config.operator != operator.to_string()
        || config.recipient != payout.recipient
        || config.amount != binding.policy.price_micro_usd
        || config.decimals != 6
        || config.splits.len() != payout.splits.len()
        || config
            .splits
            .iter()
            .zip(&payout.splits)
            .any(|(a, b)| a.recipient.to_string() != b.recipient || a.bps != b.bps)
    {
        return Err(binding_error(
            "snapshot differs from immutable payout or worker network",
        ));
    }
    Ok(())
}

fn bound_preimage(
    config: &SessionConfig,
    onchain: &channel::DecodedChannel,
) -> Result<channel::DistributionPreimage, JobError> {
    let recipients = config
        .splits
        .iter()
        .map(|split| payment_channels::Distribution {
            recipient: split.recipient,
            bps: split.bps,
        })
        .collect::<Vec<_>>();
    channel::distribution_preimage_from_snapshot(&recipients, &onchain.channel.distribution_hash)
}

fn verify_bound_channel(
    config: &SessionConfig,
    state: &ChannelState,
    onchain: &channel::DecodedChannel,
) -> Result<(), JobError> {
    if session_open_is_terminal(state)
        || onchain.address.to_string() != state.channel_id
        || onchain.payee().to_string() != config.recipient
        || onchain.payer().to_string() != state.payer
        || onchain.mint().to_string() != config.currency
        || onchain.open_slot() != state.open_slot.unwrap_or_default()
        || Pubkey::from(onchain.channel.authorized_signer.to_bytes()).to_string()
            != state.authorized_signer
    {
        return Err(binding_error(
            "on-chain channel differs from durable snapshot",
        ));
    }
    bound_preimage(config, onchain)?;
    Ok(())
}

async fn run_session(runtime: &SettlementRuntime) -> Result<SettleSessionsMetrics, JobError> {
    let started_at = Instant::now();
    let Some(session) = runtime.session.as_ref() else {
        return Ok(SettleSessionsMetrics {
            scheme: "mpp/session",
            outcome: "disabled",
            duration_seconds: started_at.elapsed().as_secs_f64(),
            ..SettleSessionsMetrics::default()
        });
    };
    let Some(lock) = SettlementLock::acquire(
        &session.redis_url,
        SESSION_SETTLEMENT_LOCK_KEY,
        runtime.lock_ttl,
    )
    .await?
    else {
        info!(
            event = "settle_sessions_lease_contended",
            "another settle-sessions execution owns the Redis lease; skipping"
        );
        return Ok(SettleSessionsMetrics {
            scheme: "mpp/session",
            outcome: "lease_contended",
            lease_contended: 1,
            duration_seconds: started_at.elapsed().as_secs_f64(),
            ..SettleSessionsMetrics::default()
        });
    };
    let channels = match runtime
        .session
        .as_ref()
        .expect("session checked above")
        .store
        .list_channels()
        .await
    {
        Ok(channels) => channels,
        Err(error) => {
            lock.release().await;
            return Err(JobError::Config(format!("list session channels: {error}")));
        }
    };

    info!(
        dry_run = runtime.dry_run,
        network = runtime.network,
        channels = channels.len(),
        operator = %runtime.operator,
        "settle-sessions reconciliation starting"
    );

    let channels_scanned = channels.len();
    let mut candidates = Vec::new();
    let mut skipped = 0_usize;
    let mut failures = 0_usize;
    let mut finalized = 0_usize;
    let mut inventory = LifecycleInventory::default();
    let mut bound_configs = HashMap::new();
    let mut bound_close_heartbeats = Vec::new();
    let mut bound_close_ownership = HashMap::new();
    for scanned_state in channels {
        if session_open_is_terminal(&scanned_state) {
            skipped += 1;
            continue;
        }
        let (scanned_state, bound_config) =
            match restore_bound_session(runtime, session, scanned_state).await {
                Ok(restored) => restored,
                Err(error) => {
                    failures += 1;
                    skipped += 1;
                    warn!(%error, "bound session recovery failed; refusing legacy fallback");
                    continue;
                }
            };
        if let Some(config) = bound_config.as_ref() {
            bound_configs.insert(scanned_state.channel_id.clone(), config.clone());
        }
        let now = unix_now();
        let now_ms = unix_now_millis();
        let scanned_state = if let Some(config) = bound_config.as_ref() {
            match observe_bound_close(runtime, &session.store, scanned_state, config, now as u64)
                .await
            {
                Ok(state) => state,
                Err(error) => {
                    failures += 1;
                    skipped += 1;
                    warn!(%error, "failed to observe bound channel close");
                    continue;
                }
            }
        } else {
            scanned_state
        };
        let state = if runtime.dry_run && channel_close_due(&scanned_state, now_ms) {
            match transition_due_close(
                Some(scanned_state.clone()),
                now_ms,
                now as u64,
                &lock.owner,
                Duration::from_secs(runtime.lock_ttl),
            ) {
                Ok(state) => state,
                Err(error) => {
                    failures += 1;
                    skipped += 1;
                    warn!(channel_id = %scanned_state.channel_id, %error, "failed to plan due session close");
                    continue;
                }
            }
        } else if channel_close_due(&scanned_state, now_ms) {
            match claim_due_close(
                &session.store,
                &scanned_state.channel_id,
                now_ms,
                now as u64,
                &lock.owner,
                Duration::from_secs(runtime.lock_ttl),
            )
            .await
            {
                Ok(state) => state,
                Err(error) => {
                    failures += 1;
                    skipped += 1;
                    warn!(
                        channel_id = %scanned_state.channel_id,
                        %error,
                        "failed to claim due session close"
                    );
                    continue;
                }
            }
        } else {
            scanned_state
        };

        if bound_config.is_some() && !runtime.dry_run && channel_close_due(&state, now_ms) {
            let (heartbeat, lost) = LeaseHeartbeat::start_bound_close(
                session.store.clone(),
                state.channel_id.clone(),
                lock.owner.clone(),
                Duration::from_secs(runtime.lock_ttl),
            );
            bound_close_heartbeats.push(heartbeat);
            bound_close_ownership.insert(state.channel_id.clone(), lost);
        }
        let resource_state = state.clone();
        let cleanup_ready =
            match ensure_resource_cleanup(runtime, &session.store, &state, false).await {
                Ok(ready) => ready,
                Err(error) => {
                    failures += 1;
                    warn!(
                        channel_id = %state.channel_id,
                        %error,
                        "failed to garbage-collect channel-funded resources; will retry"
                    );
                    false
                }
            };

        match reconcile_channel(
            &runtime.rpc,
            &runtime.rpc_url,
            state,
            now,
            now_ms,
            &runtime.operator,
            &runtime.treasury_owner,
            bound_config.as_ref(),
        )
        .await
        {
            Ok(result) => {
                if let Some(settled_base_units) = result.stablecoin_settled_base_units {
                    telemetry::record_settle_sessions_channel_settled(
                        &result.channel_id,
                        settled_base_units,
                    );
                }
                if let Some(distributed_base_units) = result.stablecoin_distributed_base_units {
                    telemetry::record_settle_sessions_channel_distributed(
                        &result.channel_id,
                        distributed_base_units,
                    );
                }
                if let Some(escrow_active) = result.escrow_active {
                    telemetry::record_settle_sessions_channel_escrow_active(
                        &result.channel_id,
                        escrow_active,
                    );
                }
                inventory.record(result.snapshot);
                match result.store_disposition {
                    StoreDisposition::Keep => {}
                    StoreDisposition::Expire { newly_finalized } => {
                        finalized += usize::from(newly_finalized);
                        if !runtime.dry_run
                            && let Err(error) =
                                session.store.mark_finalized(&result.channel_id).await
                        {
                            failures += 1;
                            warn!(
                                channel_id = %result.channel_id,
                                %error,
                                "failed to mark finalized session channel in Redis"
                            );
                        }
                    }
                    StoreDisposition::Delete => {
                        finalized += 1;
                        let cleanup_ready = if resource_cleanup_required(
                            &resource_state,
                            unix_now_millis(),
                        ) {
                            cleanup_ready
                        } else {
                            match ensure_resource_cleanup(
                                runtime,
                                &session.store,
                                &resource_state,
                                true,
                            )
                            .await
                            {
                                Ok(ready) => ready,
                                Err(error) => {
                                    failures += 1;
                                    warn!(
                                        channel_id = %result.channel_id,
                                        %error,
                                        "failed to garbage-collect resources for absent channel; will retry"
                                    );
                                    false
                                }
                            }
                        };
                        if runtime.dry_run {
                            info!(
                                channel_id = %result.channel_id,
                                "would delete Redis session for absent on-chain channel"
                            );
                        } else if cleanup_ready {
                            match session.store.delete_channel(&result.channel_id).await {
                                Ok(()) => info!(
                                    channel_id = %result.channel_id,
                                    "deleted Redis session for absent on-chain channel"
                                ),
                                Err(error) => {
                                    failures += 1;
                                    warn!(
                                        channel_id = %result.channel_id,
                                        %error,
                                        "failed to delete Redis session for absent on-chain channel"
                                    );
                                }
                            }
                        } else {
                            warn!(
                                channel_id = %result.channel_id,
                                "retaining Redis session until resource cleanup succeeds"
                            );
                        }
                    }
                }
                match result.candidate {
                    Some(candidate) => candidates.push(candidate),
                    None => skipped += 1,
                }
            }
            Err(error) => {
                failures += 1;
                skipped += 1;
                warn!(%error, "session channel reconciliation failed; skipping");
            }
        }
    }

    if runtime.dry_run {
        let planned = candidates.len();
        let idle_close_planned = candidates
            .iter()
            .filter(|candidate| candidate.kind == CandidateKind::IdleClose)
            .count();
        let watermark_planned = planned.saturating_sub(idle_close_planned);
        let metrics = SettleSessionsMetrics {
            scheme: "mpp/session",
            outcome: "dry_run",
            channels_scanned,
            watermark_planned,
            idle_close_planned,
            idle_closed: 0,
            finalized,
            transactions: 0,
            skipped,
            failures,
            opened_zero_settlements: inventory.opened_zero_settlements,
            unsealed: inventory.unsealed,
            rent_unclaimed: inventory.rent_unclaimed,
            stablecoin_settled_base_units: inventory.stablecoin_settled_base_units,
            stablecoin_undistributed_base_units: inventory.stablecoin_undistributed_base_units,
            stablecoin_distributed_base_units: inventory.stablecoin_distributed_base_units,
            stablecoin_unsettled_base_units: inventory.stablecoin_unsettled_base_units,
            redis_chain_mismatches: inventory.redis_chain_mismatches,
            lease_contended: 0,
            duration_seconds: started_at.elapsed().as_secs_f64(),
            claims: 0,
            payouts: 0,
            closes_finalized: 0,
            reclaims: 0,
        };
        lock.release().await;
        return Ok(metrics);
    }

    let planned = candidates.len();
    let idle_close_planned = candidates
        .iter()
        .filter(|candidate| candidate.kind == CandidateKind::IdleClose)
        .count();
    let watermark_planned = planned.saturating_sub(idle_close_planned);
    let handle = spawn(
        SettlementConfig::new(runtime.operator, Arc::clone(&runtime.signer)),
        Arc::new(RpcBroadcaster::new(runtime.rpc_url.clone())),
    );
    let mut submissions = JoinSet::new();
    let pipeline = TxPipeline::new(runtime.rpc_url.clone(), TxPipelineConfig::default());
    for mut candidate in candidates {
        if bound_configs.contains_key(&candidate.channel_id)
            && candidate.kind == CandidateKind::IdleClose
        {
            let Some(lost) = bound_close_ownership.get(&candidate.channel_id) else {
                failures += 1;
                continue;
            };
            let renew = || async {
                if lost.is_cancelled() {
                    return Err(TxPipelineError::SubmissionGuardRejected);
                }
                claim_due_close(
                    &session.store,
                    &candidate.channel_id,
                    unix_now_millis(),
                    unix_now() as u64,
                    &lock.owner,
                    Duration::from_secs(runtime.lock_ttl),
                )
                .await
                .map_err(|_| TxPipelineError::SubmissionGuardRejected)
            };
            let refreshed = async {
                let state = renew().await.map_err(binding_error)?;
                reconcile_channel(
                    &runtime.rpc,
                    &runtime.rpc_url,
                    state,
                    unix_now(),
                    unix_now_millis(),
                    &runtime.operator,
                    &runtime.treasury_owner,
                    bound_configs.get(&candidate.channel_id),
                )
                .await?
                .candidate
                .ok_or_else(|| binding_error("close no longer actionable"))
            }
            .await;
            match refreshed {
                Ok(value) => candidate = value,
                Err(error) => {
                    failures += 1;
                    warn!(channel_id = %candidate.channel_id, %error, "bound close refresh failed");
                    continue;
                }
            }
            let guard = || async {
                if lost.is_cancelled() {
                    return Err(TxPipelineError::SubmissionGuardRejected);
                }
                let state = claim_due_close(
                    &session.store,
                    &candidate.channel_id,
                    unix_now_millis(),
                    unix_now() as u64,
                    &lock.owner,
                    Duration::from_secs(runtime.lock_ttl),
                )
                .await
                .map_err(|_| TxPipelineError::SubmissionGuardRejected)?;
                let (state, config) = restore_bound_session(runtime, session, state)
                    .await
                    .map_err(|_| TxPipelineError::SubmissionGuardRejected)?;
                let config = config.ok_or(TxPipelineError::SubmissionGuardRejected)?;
                let fresh = reconcile_channel(
                    &runtime.rpc,
                    &runtime.rpc_url,
                    state,
                    unix_now(),
                    unix_now_millis(),
                    &runtime.operator,
                    &runtime.treasury_owner,
                    Some(&config),
                )
                .await
                .map_err(|_| TxPipelineError::SubmissionGuardRejected)?;
                if lost.is_cancelled()
                    || fresh
                        .candidate
                        .is_none_or(|fresh| fresh.instructions != candidate.instructions)
                {
                    return Err(TxPipelineError::SubmissionGuardRejected);
                }
                // Chain reads can outlast the lease. Renew after them too,
                // immediately before handing control back to the send boundary.
                let latest = claim_due_close(
                    &session.store,
                    &candidate.channel_id,
                    unix_now_millis(),
                    unix_now() as u64,
                    &lock.owner,
                    Duration::from_secs(runtime.lock_ttl),
                )
                .await
                .map_err(|_| TxPipelineError::SubmissionGuardRejected)?;
                if lost.is_cancelled() || channel_binding(&latest).ok().flatten().is_none() {
                    return Err(TxPipelineError::SubmissionGuardRejected);
                }
                Ok(())
            };
            // No independent settlement queue: the guard runs after pipeline
            // pacing and again before every send attempt. A sent transaction
            // cannot be retracted if ownership changes afterwards.
            let result: Result<String, String> = async {
                let blockhash = pipeline
                    .latest_blockhash()
                    .await
                    .map_err(|e| e.to_string())?;
                let mut transaction = pay_kit::core::tx::build_unsigned(
                    pay_kit::core::tx::TxVersion::V0,
                    &runtime.operator,
                    &candidate.instructions,
                    blockhash,
                    SettlementConfig::new(runtime.operator, runtime.signer.clone())
                        .compute_budget
                        .as_ref(),
                )
                .map_err(|e| e.to_string())?;
                guard().await.map_err(|e| e.to_string())?;
                pay_kit::core::signing::sign_versioned_transaction_slot(
                    runtime.signer.as_ref(),
                    &mut transaction,
                )
                .await
                .map_err(|e| e.to_string())?;
                pipeline
                    .broadcast_verified_guarded(&transaction, guard)
                    .await
                    .map(|signature| signature.to_string())
                    .map_err(|e| e.to_string())
            }
            .await;
            submissions.spawn(async move {
                (
                    candidate.channel_id,
                    candidate.kind,
                    candidate.before,
                    candidate.after,
                    result,
                )
            });
            continue;
        }
        let handle = handle.clone();
        submissions.spawn(async move {
            let SettlementCandidate {
                channel_id,
                instructions,
                kind,
                before,
                after,
            } = candidate;
            let result = handle.settle(channel_id.clone(), instructions).await;
            (channel_id, kind, before, after, result)
        });
    }
    drop(handle);

    let mut submissions_by_signature: HashMap<
        String,
        Vec<(
            String,
            CandidateKind,
            ChannelInventorySnapshot,
            ChannelInventorySnapshot,
        )>,
    > = HashMap::new();
    while let Some(joined) = submissions.join_next().await {
        match joined {
            Ok((channel_id, kind, before, after, Ok(signature))) => {
                info!(%channel_id, ?kind, %signature, "session lifecycle transaction broadcast");
                submissions_by_signature
                    .entry(signature)
                    .or_default()
                    .push((channel_id, kind, before, after));
            }
            Ok((channel_id, kind, _, _, Err(error))) => {
                failures += 1;
                error!(%channel_id, ?kind, %error, "session lifecycle broadcast failed");
            }
            Err(error) => {
                failures += 1;
                error!(%error, "session settlement task failed");
            }
        }
    }

    let mut idle_closed = 0_usize;
    let mut transactions = 0_usize;
    for (signature, submitted) in &submissions_by_signature {
        if let Err(error) = runtime
            .rpc
            .confirm_signature(&runtime.rpc_url, signature, runtime.confirm_timeout)
            .await
        {
            failures += 1;
            error!(%signature, %error, "session settlement confirmation failed");
            continue;
        }
        transactions += 1;

        for (channel_id, kind, before, after) in submitted {
            inventory.replace(*before, *after);
            telemetry::record_settle_sessions_channel_settled(
                channel_id,
                after.stablecoin_settled_base_units,
            );
            telemetry::record_settle_sessions_channel_distributed(
                channel_id,
                after.stablecoin_distributed_base_units,
            );
            telemetry::record_settle_sessions_channel_escrow_active(
                channel_id,
                !after.rent_unclaimed,
            );
            if *kind != CandidateKind::IdleClose {
                continue;
            }
            if bound_configs.contains_key(channel_id) {
                match session
                    .store
                    .update_channel(
                        channel_id,
                        Box::new(|current| {
                            let mut state = current
                                .ok_or_else(|| StoreError::Internal("Channel not found".into()))?;
                            state.sealed = true;
                            Ok(state)
                        }),
                    )
                    .await
                {
                    Ok(_) => idle_closed += 1,
                    Err(error) => {
                        failures += 1;
                        error!(%channel_id, %error, "closed bound channel but failed to retain sealed ownership record");
                    }
                }
                continue;
            }
            match session.store.mark_finalized(channel_id).await {
                Ok(()) => idle_closed += 1,
                Err(error) => {
                    failures += 1;
                    error!(
                        %channel_id,
                        %error,
                        "closed session channel but failed to mark it sealed in Redis"
                    );
                }
            }
        }
    }

    let metrics = SettleSessionsMetrics {
        scheme: "mpp/session",
        outcome: if failures == 0 { "succeeded" } else { "failed" },
        channels_scanned,
        watermark_planned,
        idle_close_planned,
        idle_closed,
        finalized,
        transactions,
        skipped,
        failures,
        opened_zero_settlements: inventory.opened_zero_settlements,
        unsealed: inventory.unsealed,
        rent_unclaimed: inventory.rent_unclaimed,
        stablecoin_settled_base_units: inventory.stablecoin_settled_base_units,
        stablecoin_undistributed_base_units: inventory.stablecoin_undistributed_base_units,
        stablecoin_distributed_base_units: inventory.stablecoin_distributed_base_units,
        stablecoin_unsettled_base_units: inventory.stablecoin_unsettled_base_units,
        redis_chain_mismatches: inventory.redis_chain_mismatches,
        lease_contended: 0,
        duration_seconds: started_at.elapsed().as_secs_f64(),
        claims: 0,
        payouts: 0,
        closes_finalized: 0,
        reclaims: 0,
    };
    for heartbeat in bound_close_heartbeats {
        heartbeat.shutdown().await;
    }
    lock.release().await;
    Ok(metrics)
}

async fn run_batch(runtime: &SettlementRuntime) -> Result<SettleSessionsMetrics, JobError> {
    let started_at = Instant::now();
    let batch = runtime
        .batch
        .as_ref()
        .expect("batch runtime checked by caller");
    let Some(lock) = SettlementLock::acquire(
        &batch.redis_url,
        BATCH_SETTLEMENT_LOCK_KEY,
        runtime.lock_ttl,
    )
    .await?
    else {
        info!(
            event = "settle_batch_lease_contended",
            "another batch-settlement execution owns the Redis lease; skipping"
        );
        return Ok(SettleSessionsMetrics {
            scheme: "x402/batch",
            outcome: "lease_contended",
            lease_contended: 1,
            duration_seconds: started_at.elapsed().as_secs_f64(),
            ..SettleSessionsMetrics::default()
        });
    };

    let channels = match batch.store.list_channels().await {
        Ok(channels) => channels,
        Err(error) => {
            lock.release().await;
            return Err(JobError::Config(format!("list batch channels: {error}")));
        }
    };
    info!(
        dry_run = runtime.dry_run,
        network = runtime.network,
        channels = channels.len(),
        operator = %runtime.operator,
        settle_active_channels = batch.settle_active_channels,
        distribution_threshold_base_units = batch.distribution_threshold_base_units,
        settlement_max_idle_seconds = batch.settlement_max_idle.as_secs(),
        snapshot_max_age_seconds = batch.snapshot_max_age.as_secs(),
        reconciliation_concurrency = batch.reconciliation_concurrency,
        "x402 batch-settlement reconciliation starting"
    );
    if channels.is_empty() {
        let metrics = SettleSessionsMetrics {
            scheme: "x402/batch",
            outcome: if runtime.dry_run {
                "dry_run"
            } else {
                "succeeded"
            },
            duration_seconds: started_at.elapsed().as_secs_f64(),
            ..SettleSessionsMetrics::default()
        };
        lock.release().await;
        return Ok(metrics);
    }
    let current_slot = match runtime.rpc.get_slot(&runtime.rpc_url).await {
        Ok(slot) => slot,
        Err(error) => {
            lock.release().await;
            return Err(JobError::Rpc(error));
        }
    };
    let mut metrics = SettleSessionsMetrics {
        scheme: "x402/batch",
        channels_scanned: channels.len(),
        ..SettleSessionsMetrics::default()
    };
    let mut candidates = Vec::new();
    let mut inventory = LifecycleInventory::default();
    let mut due_channels = Vec::new();
    let now = unix_now();
    for state in channels {
        if !channel::is_recorded_legacy(Some(&state)).unwrap_or(false) {
            metrics.failures += 1;
            metrics.skipped += 1;
            warn!(channel_id = %state.channel_id, "bound or malformed session in batch namespace; refusing static batch settlement");
            continue;
        }
        if !batch_reconciliation_due(
            &state,
            now,
            batch.settlement_max_idle,
            batch.snapshot_max_age,
            batch.settle_active_channels,
            batch.distribution_threshold_base_units,
        ) {
            metrics.skipped += 1;
            continue;
        }
        due_channels.push(state);
    }
    let channel_batches = due_channels
        .chunks(100)
        .map(<[ChannelState]>::to_vec)
        .collect::<Vec<_>>();
    let fetched_batches = stream::iter(channel_batches.into_iter().map(|states| async move {
        let count = states.len();
        let fetched = async {
            let addresses = states
                .iter()
                .map(|state| {
                    Pubkey::from_str(&state.channel_id)
                        .map_err(|_| JobError::InvalidAddress(state.channel_id.clone()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let channels =
                channel::fetch_channels(&runtime.rpc, &runtime.rpc_url, &addresses).await?;
            Ok::<_, JobError>(states.into_iter().zip(channels).collect::<Vec<_>>())
        }
        .await;
        (count, fetched)
    }))
    // Each request reads 100 accounts. A smaller request-level fanout avoids
    // overwhelming RPC providers while still keeping the account scan broad.
    .buffer_unordered(batch.reconciliation_concurrency.clamp(1, 32))
    .collect::<Vec<_>>()
    .await;
    let mut fetched_channels = Vec::with_capacity(metrics.channels_scanned);
    for (batch_size, fetched_batch) in fetched_batches {
        match fetched_batch {
            Ok(channels) => fetched_channels.extend(channels),
            Err(error) => {
                metrics.failures += batch_size;
                metrics.skipped += batch_size;
                warn!(%error, "batch channel account fetch failed; skipping batch");
            }
        }
    }

    let mints = fetched_channels
        .iter()
        .filter_map(|(_, channel)| channel.as_ref().map(channel::DecodedChannel::mint))
        .collect::<HashSet<_>>();
    let resolved_token_programs = stream::iter(mints.into_iter().map(|mint| async move {
        (
            mint,
            channel::resolve_token_program(&runtime.rpc, &runtime.rpc_url, &mint).await,
        )
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;
    let mut token_programs = HashMap::new();
    for (mint, resolved) in resolved_token_programs {
        match resolved {
            Ok(token_program) => {
                token_programs.insert(mint, token_program);
            }
            Err(error) => warn!(%mint, %error, "failed to resolve batch channel token program"),
        }
    }

    let reconciliations = stream::iter(fetched_channels.into_iter().map(|(state, onchain)| {
        reconcile_batch_channel(
            &runtime.rpc,
            &runtime.rpc_url,
            state,
            onchain,
            now,
            current_slot,
            &runtime.operator,
            &runtime.treasury_owner,
            &token_programs,
            batch.distribution_threshold_base_units,
        )
    }))
    .buffer_unordered(batch.reconciliation_concurrency)
    .collect::<Vec<_>>()
    .await;
    for reconciliation in reconciliations {
        match reconciliation {
            Ok(result) => {
                if let Some(settled_on_chain) = result.observed_settled_on_chain
                    && let Err(error) = update_batch_settled_watermark(
                        &batch.store,
                        &result.channel_id,
                        settled_on_chain,
                        now.max(0) as u64,
                    )
                    .await
                {
                    metrics.failures += 1;
                    warn!(channel_id = %result.channel_id, %error, "failed to persist observed batch settlement watermark");
                }
                if let Some(settled) = result.stablecoin_settled_base_units {
                    telemetry::record_channel_settled("x402/batch", &result.channel_id, settled);
                }
                if let Some(distributed) = result.stablecoin_distributed_base_units {
                    telemetry::record_channel_distributed(
                        "x402/batch",
                        &result.channel_id,
                        distributed,
                    );
                }
                if let Some(active) = result.escrow_active {
                    telemetry::record_channel_escrow_active(
                        "x402/batch",
                        &result.channel_id,
                        active,
                    );
                }
                inventory.record(result.snapshot);
                if let Some(candidate) = result.candidate {
                    candidates.push(candidate);
                } else {
                    metrics.skipped += 1;
                }
                if result.delete_absent
                    && !runtime.dry_run
                    && let Err(error) = batch.store.delete_channel(&result.channel_id).await
                {
                    metrics.failures += 1;
                    warn!(channel_id = %result.channel_id, %error, "failed to delete absent batch channel");
                }
            }
            Err(error) => {
                metrics.failures += 1;
                metrics.skipped += 1;
                warn!(%error, "batch channel reconciliation failed; skipping");
            }
        }
    }

    metrics.claims = candidates
        .iter()
        .filter(|candidate| {
            matches!(
                candidate.kind,
                BatchCandidateKind::Claim | BatchCandidateKind::ClaimAndPayout
            )
        })
        .count();
    metrics.payouts = candidates
        .iter()
        .filter(|candidate| {
            matches!(
                candidate.kind,
                BatchCandidateKind::ClaimAndPayout | BatchCandidateKind::Payout
            )
        })
        .count();
    metrics.closes_finalized = candidates
        .iter()
        .filter(|candidate| candidate.kind == BatchCandidateKind::FinalizeClose)
        .count();
    metrics.reclaims = candidates
        .iter()
        .filter(|candidate| candidate.kind == BatchCandidateKind::Reclaim)
        .count();

    if runtime.dry_run {
        metrics.outcome = "dry_run";
        metrics.duration_seconds = started_at.elapsed().as_secs_f64();
        lock.release().await;
        return Ok(metrics);
    }

    let pipeline = TxPipeline::new(runtime.rpc_url.clone(), TxPipelineConfig::default());
    // Version 1 when the cluster gate is active: 18 voucher settlements per
    // flush transaction instead of 4.
    let tx_version = {
        let rpc_url = runtime.rpc_url.clone();
        tokio::task::spawn_blocking(move || {
            pay_kit::core::tx::highest(
                &pay_kit::core::tx::TxV1Mode::Auto
                    .resolve(&pay_kit::mpp::solana_rpc_client::rpc_client::RpcClient::new(rpc_url)),
            )
        })
        .await
        .unwrap_or(pay_kit::core::tx::TxVersion::V0)
    };
    let mut settlement_config =
        SettlementConfig::new(runtime.operator, Arc::clone(&runtime.signer))
            .with_tx_version(tx_version);
    if tx_version == pay_kit::core::tx::TxVersion::V1 {
        // Fifteen distribution settlements can introduce 67 distinct static
        // accounts. Keep V1 enabled, but cap this heterogeneous workload below
        // its 64-account ceiling until pay-kit packs against compiled messages.
        settlement_config.max_voucher_settlements_per_tx =
            settlement_config.max_voucher_settlements_per_tx.min(14);
    }
    let handle = spawn(
        settlement_config,
        Arc::new(RpcBroadcaster::with_pipeline(pipeline.clone())),
    );
    let mut submissions = JoinSet::new();
    for candidate in candidates {
        let handle = handle.clone();
        submissions.spawn(async move {
            let result = handle
                .settle(candidate.channel_id.clone(), candidate.instructions.clone())
                .await;
            (candidate, result)
        });
    }
    drop(handle);

    let mut submissions_by_signature: HashMap<String, Vec<BatchCandidate>> = HashMap::new();
    while let Some(joined) = submissions.join_next().await {
        match joined {
            Ok((candidate, Ok(signature))) => {
                submissions_by_signature
                    .entry(signature)
                    .or_default()
                    .push(candidate);
            }
            Ok((candidate, Err(error))) => {
                metrics.failures += 1;
                warn!(channel_id = %candidate.channel_id, %error, "batch lifecycle broadcast failed");
            }
            Err(error) => {
                metrics.failures += 1;
                warn!(%error, "batch lifecycle task failed");
            }
        }
    }

    let confirmations = stream::iter(submissions_by_signature.into_iter().map(
        |(signature, submitted)| {
            let pipeline = pipeline.clone();
            async move {
                let confirmation = match Signature::from_str(&signature) {
                    Ok(signature) => pipeline
                        .confirm(signature)
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(format!("invalid settlement signature: {error}")),
                };
                (signature, submitted, confirmation)
            }
        },
    ))
    .buffer_unordered(256)
    .collect::<Vec<_>>()
    .await;

    for (signature, submitted, confirmation) in confirmations {
        if let Err(error) = confirmation {
            metrics.failures += submitted.len();
            warn!(%signature, %error, channels = submitted.len(), "batch lifecycle confirmation failed");
            continue;
        }
        metrics.transactions += 1;
        for candidate in submitted {
            inventory.replace(candidate.before, candidate.after);
            telemetry::record_channel_settled(
                "x402/batch",
                &candidate.channel_id,
                candidate.after.stablecoin_settled_base_units,
            );
            telemetry::record_channel_distributed(
                "x402/batch",
                &candidate.channel_id,
                candidate.after.stablecoin_distributed_base_units,
            );
            telemetry::record_channel_escrow_active(
                "x402/batch",
                &candidate.channel_id,
                candidate.after.unsealed,
            );
            if let Err(error) = update_batch_settled_watermark(
                &batch.store,
                &candidate.channel_id,
                candidate.settled_on_chain,
                unix_now().max(0) as u64,
            )
            .await
            {
                metrics.failures += 1;
                warn!(channel_id = %candidate.channel_id, %error, "failed to persist confirmed batch settlement watermark");
            }
            match candidate.store_action {
                BatchStoreAction::Keep => {}
                BatchStoreAction::MarkFinalized => {
                    if let Err(error) = batch.store.mark_finalized(&candidate.channel_id).await {
                        metrics.failures += 1;
                        warn!(channel_id = %candidate.channel_id, %error, "failed to mark finalized batch channel");
                    }
                }
                BatchStoreAction::Delete => {
                    if let Err(error) = batch.store.delete_channel(&candidate.channel_id).await {
                        metrics.failures += 1;
                        warn!(channel_id = %candidate.channel_id, %error, "failed to delete reclaimed batch channel");
                    }
                }
            }
            info!(
                scheme = "x402/batch",
                stage = candidate.kind.label(),
                channel_id = %candidate.channel_id,
                %signature,
                "batch channel lifecycle transaction confirmed"
            );
        }
    }

    metrics.opened_zero_settlements = inventory.opened_zero_settlements;
    metrics.unsealed = inventory.unsealed;
    metrics.rent_unclaimed = inventory.rent_unclaimed;
    metrics.stablecoin_settled_base_units = inventory.stablecoin_settled_base_units;
    metrics.stablecoin_undistributed_base_units = inventory.stablecoin_undistributed_base_units;
    metrics.stablecoin_distributed_base_units = inventory.stablecoin_distributed_base_units;
    metrics.stablecoin_unsettled_base_units = inventory.stablecoin_unsettled_base_units;
    metrics.redis_chain_mismatches = inventory.redis_chain_mismatches;
    metrics.outcome = if metrics.failures == 0 {
        "succeeded"
    } else {
        "failed"
    };
    metrics.duration_seconds = started_at.elapsed().as_secs_f64();
    lock.release().await;
    Ok(metrics)
}

fn log_summary(metrics: &SettleSessionsMetrics) {
    info!(
        event = "settle_sessions_summary",
        scheme = metrics.scheme,
        outcome = metrics.outcome,
        channels_scanned = metrics.channels_scanned,
        planned = metrics
            .watermark_planned
            .saturating_add(metrics.idle_close_planned),
        watermark_planned = metrics.watermark_planned,
        idle_close_planned = metrics.idle_close_planned,
        idle_closed = metrics.idle_closed,
        finalized = metrics.finalized,
        transactions = metrics.transactions,
        skipped = metrics.skipped,
        failures = metrics.failures,
        opened_zero_settlements = metrics.opened_zero_settlements,
        unsealed = metrics.unsealed,
        rent_unclaimed = metrics.rent_unclaimed,
        stablecoin_settled_base_units = metrics.stablecoin_settled_base_units,
        stablecoin_undistributed_base_units = metrics.stablecoin_undistributed_base_units,
        stablecoin_distributed_base_units = metrics.stablecoin_distributed_base_units,
        stablecoin_unsettled_base_units = metrics.stablecoin_unsettled_base_units,
        redis_chain_mismatches = metrics.redis_chain_mismatches,
        lease_contended = metrics.lease_contended,
        claims = metrics.claims,
        payouts = metrics.payouts,
        closes_finalized = metrics.closes_finalized,
        reclaims = metrics.reclaims,
        duration_ms = (metrics.duration_seconds * 1_000.0) as u64,
        "settle-sessions summary"
    );
}

fn resource_cleanup_required(state: &ChannelState, now_ms: u64) -> bool {
    state.sealed
        || state.close_requested_at.is_some()
        || state.final_cumulative.is_some()
        || (state.deposit > 0
            && state.cumulative >= state.deposit
            && state
                .lifecycle
                .as_ref()
                .is_some_and(|lifecycle| lifecycle.close_after <= now_ms))
}

fn cleanup_timestamp(state: &ChannelState, key: &str) -> Option<u64> {
    state.extra.get(key).and_then(serde_json::Value::as_u64)
}

fn record_resource_cleanup(
    state: &mut ChannelState,
    now_ms: u64,
    quiet_ms: u64,
    resources: usize,
) -> bool {
    state.extra.insert(
        RESOURCE_CLEANUP_LAST_AT.into(),
        serde_json::Value::from(now_ms),
    );
    if resources == 0 {
        let empty_since = cleanup_timestamp(state, RESOURCE_CLEANUP_EMPTY_SINCE).unwrap_or(now_ms);
        state.extra.insert(
            RESOURCE_CLEANUP_EMPTY_SINCE.into(),
            serde_json::Value::from(empty_since),
        );
        if now_ms.saturating_sub(empty_since) >= quiet_ms {
            state.extra.insert(
                RESOURCE_CLEANUP_COMPLETED_AT.into(),
                serde_json::Value::from(now_ms),
            );
        }
    } else {
        state.extra.remove(RESOURCE_CLEANUP_EMPTY_SINCE);
    }
    state.extra.contains_key(RESOURCE_CLEANUP_COMPLETED_AT)
}

async fn ensure_resource_cleanup(
    runtime: &SettlementRuntime,
    store: &RedisChannelStore,
    state: &ChannelState,
    force: bool,
) -> Result<bool, JobError> {
    // Deployment sessions fund invocation service, not fleet-owned resources.
    if !channel::is_recorded_legacy(Some(state))? {
        return Ok(false);
    }
    if (!force && !resource_cleanup_required(state, unix_now_millis()))
        || !runtime.resource_cleaner.is_enabled()
    {
        return Ok(true);
    }
    if state.extra.contains_key(RESOURCE_CLEANUP_COMPLETED_AT) {
        return Ok(true);
    }
    if runtime.dry_run {
        info!(
            channel_id = %state.channel_id,
            "would garbage-collect resources funded by unusable session channel"
        );
        return Ok(false);
    }

    let now_ms = unix_now_millis();
    let interval_ms =
        u64::try_from(runtime.resource_cleanup_interval.as_millis()).unwrap_or(u64::MAX);
    if cleanup_timestamp(state, RESOURCE_CLEANUP_LAST_AT)
        .is_some_and(|last| now_ms.saturating_sub(last) < interval_ms)
    {
        return Ok(false);
    }

    let summary = runtime
        .resource_cleaner
        .cleanup_channel(&state.channel_id)
        .await?;
    let resources = summary.compute_resources + summary.data_resources + summary.trigger_resources;
    let quiet_ms = u64::try_from(runtime.resource_cleanup_quiet.as_millis()).unwrap_or(u64::MAX);
    let updated = store
        .update_channel(
            &state.channel_id,
            Box::new(move |current| {
                let mut state = current.ok_or_else(|| {
                    StoreError::Internal("channel disappeared during resource cleanup".into())
                })?;
                record_resource_cleanup(&mut state, now_ms, quiet_ms, resources);
                Ok(state)
            }),
        )
        .await
        .map_err(|error| JobError::Config(format!("mark resource cleanup complete: {error}")))?;
    info!(
        channel_id = %state.channel_id,
        compute_resources = summary.compute_resources,
        data_resources = summary.data_resources,
        trigger_resources = summary.trigger_resources,
        "garbage-collected resources funded by unusable session channel"
    );
    Ok(updated.extra.contains_key(RESOURCE_CLEANUP_COMPLETED_AT))
}

#[derive(Default)]
struct LifecycleInventory {
    opened_zero_settlements: usize,
    unsealed: usize,
    rent_unclaimed: usize,
    stablecoin_settled_base_units: u64,
    stablecoin_undistributed_base_units: u64,
    stablecoin_distributed_base_units: u64,
    stablecoin_unsettled_base_units: u64,
    redis_chain_mismatches: usize,
}

impl LifecycleInventory {
    fn record(&mut self, snapshot: ChannelInventorySnapshot) {
        self.opened_zero_settlements += usize::from(snapshot.opened_zero_settlements);
        self.unsealed += usize::from(snapshot.unsealed);
        self.rent_unclaimed += usize::from(snapshot.rent_unclaimed);
        self.stablecoin_settled_base_units = self
            .stablecoin_settled_base_units
            .saturating_add(snapshot.stablecoin_settled_base_units);
        self.stablecoin_undistributed_base_units = self
            .stablecoin_undistributed_base_units
            .saturating_add(snapshot.stablecoin_undistributed_base_units);
        self.stablecoin_distributed_base_units = self
            .stablecoin_distributed_base_units
            .saturating_add(snapshot.stablecoin_distributed_base_units);
        self.stablecoin_unsettled_base_units = self
            .stablecoin_unsettled_base_units
            .saturating_add(snapshot.stablecoin_unsettled_base_units);
        self.redis_chain_mismatches += usize::from(snapshot.redis_chain_mismatch);
    }

    fn replace(&mut self, before: ChannelInventorySnapshot, after: ChannelInventorySnapshot) {
        self.opened_zero_settlements = self
            .opened_zero_settlements
            .saturating_sub(usize::from(before.opened_zero_settlements));
        self.unsealed = self.unsealed.saturating_sub(usize::from(before.unsealed));
        self.rent_unclaimed = self
            .rent_unclaimed
            .saturating_sub(usize::from(before.rent_unclaimed));
        self.stablecoin_settled_base_units = self
            .stablecoin_settled_base_units
            .saturating_sub(before.stablecoin_settled_base_units);
        self.stablecoin_undistributed_base_units = self
            .stablecoin_undistributed_base_units
            .saturating_sub(before.stablecoin_undistributed_base_units);
        self.stablecoin_distributed_base_units = self
            .stablecoin_distributed_base_units
            .saturating_sub(before.stablecoin_distributed_base_units);
        self.stablecoin_unsettled_base_units = self
            .stablecoin_unsettled_base_units
            .saturating_sub(before.stablecoin_unsettled_base_units);
        self.redis_chain_mismatches = self
            .redis_chain_mismatches
            .saturating_sub(usize::from(before.redis_chain_mismatch));
        self.record(after);
    }
}

#[derive(Clone, Copy, Default)]
struct ChannelInventorySnapshot {
    opened_zero_settlements: bool,
    unsealed: bool,
    rent_unclaimed: bool,
    stablecoin_settled_base_units: u64,
    stablecoin_undistributed_base_units: u64,
    stablecoin_distributed_base_units: u64,
    stablecoin_unsettled_base_units: u64,
    redis_chain_mismatch: bool,
}

struct ReconcileResult {
    channel_id: String,
    candidate: Option<SettlementCandidate>,
    store_disposition: StoreDisposition,
    snapshot: ChannelInventorySnapshot,
    stablecoin_settled_base_units: Option<u64>,
    stablecoin_distributed_base_units: Option<u64>,
    escrow_active: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreDisposition {
    Keep,
    Expire { newly_finalized: bool },
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
    Watermark,
    IdleClose,
}

struct SettlementCandidate {
    channel_id: String,
    instructions: Vec<solana_instruction::Instruction>,
    kind: CandidateKind,
    before: ChannelInventorySnapshot,
    after: ChannelInventorySnapshot,
}

struct BatchReconcileResult {
    channel_id: String,
    candidate: Option<BatchCandidate>,
    delete_absent: bool,
    snapshot: ChannelInventorySnapshot,
    stablecoin_settled_base_units: Option<u64>,
    stablecoin_distributed_base_units: Option<u64>,
    escrow_active: Option<bool>,
    observed_settled_on_chain: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchCandidateKind {
    Claim,
    ClaimAndPayout,
    Payout,
    FinalizeClose,
    Reclaim,
}

impl BatchCandidateKind {
    fn label(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::ClaimAndPayout => "claim_and_payout",
            Self::Payout => "payout",
            Self::FinalizeClose => "finalize_close",
            Self::Reclaim => "reclaim",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BatchStoreAction {
    Keep,
    MarkFinalized,
    Delete,
}

struct BatchCandidate {
    channel_id: String,
    instructions: Vec<solana_instruction::Instruction>,
    kind: BatchCandidateKind,
    store_action: BatchStoreAction,
    before: ChannelInventorySnapshot,
    after: ChannelInventorySnapshot,
    settled_on_chain: u64,
}

fn batch_reconciliation_due(
    state: &ChannelState,
    now: i64,
    max_idle: Duration,
    snapshot_max_age: Duration,
    settle_active: bool,
    distribution_threshold_base_units: Option<u64>,
) -> bool {
    if state.sealed || state.close_requested_at.is_some() {
        return true;
    }
    if distribution_threshold_base_units.is_some_and(|threshold| {
        distribution_threshold_reached(
            state.settled_on_chain,
            state.distributed_on_chain,
            threshold,
        )
    }) {
        return true;
    }
    let now_seconds = u64::try_from(now).unwrap_or_default();
    if now_seconds.saturating_sub(state.onchain_checked_at) >= snapshot_max_age.as_secs() {
        return true;
    }
    let unsettled = state.cumulative > state.settled_on_chain;
    unsettled
        && (settle_active
            || now_seconds.saturating_sub(state.last_activity_at) >= max_idle.as_secs())
}

async fn update_batch_settled_watermark(
    store: &RedisChannelStore,
    channel_id: &str,
    settled_on_chain: u64,
    checked_at: u64,
) -> Result<(), StoreError> {
    store
        .update_channel(
            channel_id,
            Box::new(move |current| {
                let mut state = current
                    .ok_or_else(|| StoreError::Internal("batch channel not found".into()))?;
                state.settled_on_chain = state.settled_on_chain.max(settled_on_chain);
                state.onchain_checked_at = state.onchain_checked_at.max(checked_at);
                Ok(state)
            }),
        )
        .await
        .map(|_| ())
}

fn inventory_snapshot(
    redis_sealed: bool,
    onchain_status: u8,
    onchain_settled: u64,
    onchain_payout_watermark: u64,
    redis_cumulative: u64,
    mint: &Pubkey,
) -> ChannelInventorySnapshot {
    let unsealed = matches!(onchain_status, STATUS_OPEN | STATUS_CLOSING);
    let onchain_distributed =
        effective_distributed_amount(onchain_status, onchain_settled, onchain_payout_watermark);
    ChannelInventorySnapshot {
        opened_zero_settlements: onchain_status == STATUS_OPEN && onchain_settled == 0,
        unsealed,
        rent_unclaimed: onchain_status == STATUS_DISTRIBUTED,
        stablecoin_settled_base_units: settled_stablecoin_base_units(mint, onchain_settled),
        stablecoin_undistributed_base_units: stablecoin_base_units(
            mint,
            onchain_settled.saturating_sub(onchain_distributed),
        ),
        stablecoin_distributed_base_units: stablecoin_base_units(mint, onchain_distributed),
        stablecoin_unsettled_base_units: unsettled_stablecoin_base_units(
            mint,
            redis_cumulative,
            onchain_settled,
        ),
        redis_chain_mismatch: redis_sealed && unsealed,
    }
}

/// Resolve the amount known to have left escrow.
///
/// A final sealed `distribute` drains and closes the escrow account before
/// marking the channel `Distributed`, but does not persist the payout
/// watermark. The terminal status therefore supersedes that stale watermark.
fn effective_distributed_amount(status: u8, settled: u64, payout_watermark: u64) -> u64 {
    if status == STATUS_DISTRIBUTED {
        settled
    } else {
        payout_watermark
    }
}

const MAINNET_STABLECOIN_MINT: Pubkey =
    solana_pubkey::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const DEVNET_STABLECOIN_MINT: Pubkey =
    solana_pubkey::pubkey!("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
const DEVNET_USDTEST_MINT: Pubkey =
    solana_pubkey::pubkey!("6MJyWHwFpPsaTEuYarEz49ngtsSKPYn6yHqtJUW2a9St");

fn unsettled_stablecoin_base_units(mint: &Pubkey, cumulative: u64, settled: u64) -> u64 {
    stablecoin_base_units(mint, cumulative.saturating_sub(settled))
}

fn settled_stablecoin_base_units(mint: &Pubkey, settled: u64) -> u64 {
    stablecoin_base_units(mint, settled)
}

fn stablecoin_base_units(mint: &Pubkey, amount: u64) -> u64 {
    stablecoin_base_units_option(mint, amount).unwrap_or_default()
}

fn stablecoin_base_units_option(mint: &Pubkey, amount: u64) -> Option<u64> {
    (*mint == MAINNET_STABLECOIN_MINT
        || *mint == DEVNET_STABLECOIN_MINT
        || *mint == DEVNET_USDTEST_MINT)
        .then_some(amount)
}

fn absent_onchain_store_disposition(state: &ChannelState) -> StoreDisposition {
    if state.open_slot.is_some() {
        StoreDisposition::Delete
    } else {
        StoreDisposition::Keep
    }
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_batch_channel(
    rpc: &RpcClient,
    rpc_url: &str,
    state: ChannelState,
    onchain: Option<channel::DecodedChannel>,
    now: i64,
    current_slot: u64,
    operator: &Pubkey,
    treasury_owner: &Pubkey,
    token_programs: &HashMap<Pubkey, Pubkey>,
    distribution_threshold_base_units: Option<u64>,
) -> Result<BatchReconcileResult, JobError> {
    let state_channel_id = state.channel_id.clone();
    let Some(onchain) = onchain else {
        return Ok(BatchReconcileResult {
            channel_id: state_channel_id,
            candidate: None,
            delete_absent: state.open_slot.is_some() && !state.has_in_flight_authorization(),
            snapshot: ChannelInventorySnapshot::default(),
            stablecoin_settled_base_units: None,
            stablecoin_distributed_base_units: None,
            escrow_active: Some(false),
            observed_settled_on_chain: None,
        });
    };
    let channel_id = onchain.address;
    if onchain.payee() != *operator || onchain.rent_payer() != *operator {
        return Err(JobError::Config(format!(
            "batch channel {} lifecycle authority differs from worker {operator}",
            state.channel_id
        )));
    }

    let snapshot = inventory_snapshot(
        state.sealed,
        onchain.channel.status,
        onchain.channel.settlement.settled,
        onchain.channel.settlement.payout_watermark,
        state.cumulative,
        &onchain.mint(),
    );
    let settled_metric =
        stablecoin_base_units_option(&onchain.mint(), onchain.channel.settlement.settled);
    let distributed = effective_distributed_amount(
        onchain.channel.status,
        onchain.channel.settlement.settled,
        onchain.channel.settlement.payout_watermark,
    );
    let distributed_metric = stablecoin_base_units_option(&onchain.mint(), distributed);
    let mut instructions = Vec::new();
    let (kind, store_action, after, confirmed_settled) = match onchain.channel.status {
        STATUS_OPEN => {
            let mut target_settled = onchain.channel.settlement.settled;
            let mut kind = BatchCandidateKind::Payout;
            if state.cumulative > target_settled {
                let authorized_signer = Pubkey::from_str(&state.authorized_signer)
                    .map_err(|_| JobError::InvalidAddress(state.authorized_signer.clone()))?;
                let onchain_signer = Pubkey::from(onchain.channel.authorized_signer.to_bytes());
                if authorized_signer != onchain_signer {
                    return Err(JobError::Config(format!(
                        "batch channel {} authorized signer differs from Redis",
                        state.channel_id
                    )));
                }
                let signature = state.highest_voucher_signature.as_deref().ok_or_else(|| {
                    JobError::TxBuild("latest batch voucher has no signature".into())
                })?;
                let expires_at = state.highest_voucher_expires_at.ok_or_else(|| {
                    JobError::TxBuild("latest batch voucher has no expiry".into())
                })?;
                if expires_at != 0 && expires_at <= now {
                    return Err(JobError::TxBuild(format!(
                        "latest batch voucher for {} expired at {expires_at}",
                        state.channel_id
                    )));
                }
                instructions.extend(
                    payment_channels::build_settle_instructions(
                        &channel_id,
                        &authorized_signer,
                        &decode_voucher_signature(signature)?,
                        state.cumulative,
                        expires_at,
                        &payment_channels::default_program_id(),
                    )
                    .map_err(|error| JobError::TxBuild(format!("settle instruction: {error}")))?,
                );
                target_settled = state.cumulative;
                kind = if distribution_threshold_base_units.is_some_and(|threshold| {
                    distribution_threshold_reached(
                        target_settled,
                        onchain.channel.settlement.payout_watermark,
                        threshold,
                    )
                }) {
                    BatchCandidateKind::ClaimAndPayout
                } else {
                    BatchCandidateKind::Claim
                };
            }
            let should_distribute = distribution_threshold_base_units.is_some_and(|threshold| {
                distribution_threshold_reached(
                    target_settled,
                    onchain.channel.settlement.payout_watermark,
                    threshold,
                )
            });
            if !should_distribute {
                if kind == BatchCandidateKind::Payout {
                    return Ok(BatchReconcileResult {
                        channel_id: state_channel_id,
                        candidate: None,
                        delete_absent: false,
                        snapshot,
                        stablecoin_settled_base_units: settled_metric,
                        stablecoin_distributed_base_units: distributed_metric,
                        escrow_active: Some(true),
                        observed_settled_on_chain: Some(onchain.channel.settlement.settled),
                    });
                }
                return Ok(BatchReconcileResult {
                    channel_id: state_channel_id.clone(),
                    candidate: Some(BatchCandidate {
                        channel_id: state_channel_id,
                        instructions,
                        kind,
                        store_action: BatchStoreAction::Keep,
                        before: snapshot,
                        after: inventory_snapshot(
                            state.sealed,
                            STATUS_OPEN,
                            target_settled,
                            onchain.channel.settlement.payout_watermark,
                            state.cumulative,
                            &onchain.mint(),
                        ),
                        settled_on_chain: target_settled,
                    }),
                    delete_absent: false,
                    snapshot,
                    stablecoin_settled_base_units: settled_metric,
                    stablecoin_distributed_base_units: distributed_metric,
                    escrow_active: Some(true),
                    observed_settled_on_chain: Some(onchain.channel.settlement.settled),
                });
            }
            if target_settled <= onchain.channel.settlement.payout_watermark {
                return Ok(BatchReconcileResult {
                    channel_id: state_channel_id,
                    candidate: None,
                    delete_absent: false,
                    snapshot,
                    stablecoin_settled_base_units: settled_metric,
                    stablecoin_distributed_base_units: distributed_metric,
                    escrow_active: Some(true),
                    observed_settled_on_chain: Some(onchain.channel.settlement.settled),
                });
            }
            let token_program = token_programs
                .get(&onchain.mint())
                .copied()
                .ok_or_else(|| {
                    JobError::TxBuild(format!(
                        "token program unavailable for batch channel mint {}",
                        onchain.mint()
                    ))
                })?;
            let preimage = channel::recover_distribution_preimage(rpc, rpc_url, &onchain).await?;
            instructions.push(
                channel::build_distribute_ix(&onchain, treasury_owner, &token_program, &preimage).0,
            );
            (
                kind,
                BatchStoreAction::Keep,
                inventory_snapshot(
                    state.sealed,
                    STATUS_OPEN,
                    target_settled,
                    target_settled,
                    state.cumulative,
                    &onchain.mint(),
                ),
                target_settled,
            )
        }
        STATUS_CLOSING if now < onchain.close_deadline() => {
            let onchain_signer = Pubkey::from(onchain.channel.authorized_signer.to_bytes());
            let (signature, target_settled, expires_at) = close_voucher(
                &state,
                onchain.channel.settlement.settled,
                &onchain_signer,
                now,
            )?;
            instructions.extend(
                payment_channels::build_settle_and_seal_instructions(
                    operator,
                    &channel_id,
                    &onchain_signer,
                    signature.as_ref(),
                    target_settled,
                    expires_at,
                    &payment_channels::default_program_id(),
                )
                .map_err(|error| {
                    JobError::TxBuild(format!("settle-and-seal instruction: {error}"))
                })?,
            );
            let token_program = token_programs
                .get(&onchain.mint())
                .copied()
                .ok_or_else(|| {
                    JobError::TxBuild(format!(
                        "token program unavailable for batch channel mint {}",
                        onchain.mint()
                    ))
                })?;
            let preimage = channel::recover_distribution_preimage(rpc, rpc_url, &onchain).await?;
            instructions.push(
                channel::build_distribute_ix(&onchain, treasury_owner, &token_program, &preimage).0,
            );
            (
                BatchCandidateKind::FinalizeClose,
                BatchStoreAction::MarkFinalized,
                inventory_snapshot(
                    true,
                    STATUS_DISTRIBUTED,
                    target_settled,
                    target_settled,
                    state.cumulative,
                    &onchain.mint(),
                ),
                target_settled,
            )
        }
        STATUS_CLOSING if now >= onchain.close_deadline() => {
            instructions.push(channel::build_seal_ix(&channel_id));
            let token_program = token_programs
                .get(&onchain.mint())
                .copied()
                .ok_or_else(|| {
                    JobError::TxBuild(format!(
                        "token program unavailable for batch channel mint {}",
                        onchain.mint()
                    ))
                })?;
            let preimage = channel::recover_distribution_preimage(rpc, rpc_url, &onchain).await?;
            instructions.push(
                channel::build_distribute_ix(&onchain, treasury_owner, &token_program, &preimage).0,
            );
            (
                BatchCandidateKind::FinalizeClose,
                BatchStoreAction::MarkFinalized,
                inventory_snapshot(
                    true,
                    STATUS_DISTRIBUTED,
                    onchain.channel.settlement.settled,
                    onchain.channel.settlement.settled,
                    state.cumulative,
                    &onchain.mint(),
                ),
                onchain.channel.settlement.settled,
            )
        }
        STATUS_SEALED => {
            let token_program = token_programs
                .get(&onchain.mint())
                .copied()
                .ok_or_else(|| {
                    JobError::TxBuild(format!(
                        "token program unavailable for batch channel mint {}",
                        onchain.mint()
                    ))
                })?;
            let preimage = channel::recover_distribution_preimage(rpc, rpc_url, &onchain).await?;
            instructions.push(
                channel::build_distribute_ix(&onchain, treasury_owner, &token_program, &preimage).0,
            );
            (
                BatchCandidateKind::Payout,
                BatchStoreAction::MarkFinalized,
                inventory_snapshot(
                    true,
                    STATUS_DISTRIBUTED,
                    onchain.channel.settlement.settled,
                    onchain.channel.settlement.settled,
                    state.cumulative,
                    &onchain.mint(),
                ),
                onchain.channel.settlement.settled,
            )
        }
        STATUS_DISTRIBUTED
            if current_slot
                > onchain
                    .open_slot()
                    .saturating_add(payment_channels::OPEN_SLOT_WINDOW) =>
        {
            instructions.push(channel::build_reclaim_ix(&channel_id, operator));
            (
                BatchCandidateKind::Reclaim,
                BatchStoreAction::Delete,
                ChannelInventorySnapshot::default(),
                onchain.channel.settlement.settled,
            )
        }
        STATUS_CLOSING | STATUS_DISTRIBUTED => {
            return Ok(BatchReconcileResult {
                channel_id: state_channel_id,
                candidate: None,
                delete_absent: false,
                snapshot,
                stablecoin_settled_base_units: settled_metric,
                stablecoin_distributed_base_units: distributed_metric,
                escrow_active: Some(onchain.channel.status != STATUS_DISTRIBUTED),
                observed_settled_on_chain: Some(onchain.channel.settlement.settled),
            });
        }
        status => {
            return Err(JobError::TxBuild(format!(
                "batch channel {} has unknown status {status}",
                state.channel_id
            )));
        }
    };

    Ok(BatchReconcileResult {
        channel_id: state_channel_id.clone(),
        candidate: Some(BatchCandidate {
            channel_id: state_channel_id,
            instructions,
            kind,
            store_action,
            before: snapshot,
            after,
            settled_on_chain: confirmed_settled,
        }),
        delete_absent: false,
        snapshot,
        stablecoin_settled_base_units: settled_metric,
        stablecoin_distributed_base_units: distributed_metric,
        escrow_active: Some(onchain.channel.status != STATUS_DISTRIBUTED),
        observed_settled_on_chain: Some(onchain.channel.settlement.settled),
    })
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_channel(
    rpc: &RpcClient,
    rpc_url: &str,
    state: ChannelState,
    now: i64,
    now_ms: u64,
    operator: &Pubkey,
    treasury_owner: &Pubkey,
    bound_config: Option<&SessionConfig>,
) -> Result<ReconcileResult, JobError> {
    let state_channel_id = state.channel_id.clone();
    let absent_disposition = absent_onchain_store_disposition(&state);
    if absent_disposition == StoreDisposition::Keep {
        return Ok(ReconcileResult {
            channel_id: state_channel_id,
            candidate: None,
            store_disposition: StoreDisposition::Keep,
            snapshot: ChannelInventorySnapshot::default(),
            stablecoin_settled_base_units: None,
            stablecoin_distributed_base_units: None,
            escrow_active: None,
        });
    }

    let channel_id = Pubkey::from_str(&state.channel_id)
        .map_err(|_| JobError::InvalidAddress(state.channel_id.clone()))?;
    let program = bound_config
        .and_then(|config| config.channel_program)
        .unwrap_or_else(payment_channels::default_program_id);
    let onchain =
        match channel::fetch_channel_for_program(rpc, rpc_url, &channel_id, &program).await {
            Ok(Some(onchain)) => onchain,
            Ok(None) => {
                return Ok(ReconcileResult {
                    channel_id: state_channel_id,
                    candidate: None,
                    store_disposition: if bound_config.is_some() {
                        StoreDisposition::Keep
                    } else {
                        absent_disposition
                    },
                    snapshot: ChannelInventorySnapshot::default(),
                    stablecoin_settled_base_units: None,
                    stablecoin_distributed_base_units: None,
                    escrow_active: Some(false),
                });
            }
            Err(error) => return Err(error),
        };

    if let Some(config) = bound_config {
        verify_bound_channel(config, &state, &onchain)?;
    }
    let snapshot = inventory_snapshot(
        state.sealed,
        onchain.channel.status,
        onchain.channel.settlement.settled,
        onchain.channel.settlement.payout_watermark,
        state.cumulative,
        &onchain.mint(),
    );
    let stablecoin_settled_base_units =
        stablecoin_base_units_option(&onchain.mint(), onchain.channel.settlement.settled);
    let distributed_amount = effective_distributed_amount(
        onchain.channel.status,
        onchain.channel.settlement.settled,
        onchain.channel.settlement.payout_watermark,
    );
    let stablecoin_distributed_base_units =
        stablecoin_base_units_option(&onchain.mint(), distributed_amount);
    let escrow_active = Some(onchain.channel.status != STATUS_DISTRIBUTED);

    if onchain.channel.status == STATUS_DISTRIBUTED {
        return Ok(ReconcileResult {
            channel_id: state_channel_id,
            candidate: None,
            store_disposition: if bound_config.is_some() {
                // Keep the ownership record: expiring it would permit the
                // fleet reclamation job to mistake it for a legacy channel.
                StoreDisposition::Keep
            } else {
                StoreDisposition::Expire {
                    newly_finalized: !state.sealed,
                }
            },
            snapshot,
            stablecoin_settled_base_units,
            stablecoin_distributed_base_units,
            escrow_active,
        });
    }

    if state.sealed {
        return Ok(ReconcileResult {
            channel_id: state_channel_id,
            candidate: None,
            store_disposition: StoreDisposition::Keep,
            snapshot,
            stablecoin_settled_base_units,
            stablecoin_distributed_base_units,
            escrow_active,
        });
    }

    // A payer may request close directly through pay-api, so the on-chain
    // state is authoritative even when this worker's Redis lifecycle did not
    // initiate the close. Settle the newest voucher and distribute promptly.
    if channel_close_due(&state, now_ms)
        || matches!(onchain.channel.status, STATUS_CLOSING | STATUS_SEALED)
    {
        if bound_config.is_some() && state.close_requested_at.is_none() {
            return Err(binding_error(
                "bound close requires the durable close claim",
            ));
        }
        let candidate = build_idle_close_candidate(
            rpc,
            rpc_url,
            &state,
            &onchain,
            now,
            operator,
            treasury_owner,
            bound_config,
        )
        .await?;
        return Ok(ReconcileResult {
            channel_id: state_channel_id,
            candidate,
            store_disposition: StoreDisposition::Keep,
            snapshot,
            stablecoin_settled_base_units,
            stablecoin_distributed_base_units,
            escrow_active,
        });
    }

    if state.cumulative == 0 {
        return Ok(ReconcileResult {
            channel_id: state_channel_id,
            candidate: None,
            store_disposition: StoreDisposition::Keep,
            snapshot,
            stablecoin_settled_base_units,
            stablecoin_distributed_base_units,
            escrow_active,
        });
    }
    let (Some(signature), Some(expires_at)) = (
        state.highest_voucher_signature.as_deref(),
        state.highest_voucher_expires_at,
    ) else {
        return Ok(ReconcileResult {
            channel_id: state_channel_id,
            candidate: None,
            store_disposition: StoreDisposition::Keep,
            snapshot,
            stablecoin_settled_base_units,
            stablecoin_distributed_base_units,
            escrow_active,
        });
    };
    if expires_at != 0 && expires_at <= now {
        warn!(
            channel_id = %state.channel_id,
            expires_at,
            "latest unsettled voucher expired"
        );
        return Ok(ReconcileResult {
            channel_id: state_channel_id,
            candidate: None,
            store_disposition: StoreDisposition::Keep,
            snapshot,
            stablecoin_settled_base_units,
            stablecoin_distributed_base_units,
            escrow_active,
        });
    }

    if onchain.channel.status != STATUS_OPEN
        || onchain.channel.settlement.settled >= state.cumulative
    {
        return Ok(ReconcileResult {
            channel_id: state_channel_id,
            candidate: None,
            store_disposition: StoreDisposition::Keep,
            snapshot,
            stablecoin_settled_base_units,
            stablecoin_distributed_base_units,
            escrow_active,
        });
    }

    let authorized_signer = Pubkey::from_str(&state.authorized_signer)
        .map_err(|_| JobError::InvalidAddress(state.authorized_signer.clone()))?;
    let onchain_signer = Pubkey::from(onchain.channel.authorized_signer.to_bytes());
    if onchain_signer != authorized_signer {
        return Err(JobError::Config(format!(
            "channel {} authorized signer differs from Redis",
            state.channel_id
        )));
    }
    let signature = decode_voucher_signature(signature)?;
    let instructions = payment_channels::build_settle_instructions(
        &channel_id,
        &authorized_signer,
        &signature,
        state.cumulative,
        expires_at,
        &program,
    )
    .map_err(|error| JobError::TxBuild(format!("settle instruction: {error}")))?;
    let after = inventory_snapshot(
        state.sealed,
        onchain.channel.status,
        state.cumulative,
        onchain.channel.settlement.payout_watermark,
        state.cumulative,
        &onchain.mint(),
    );
    Ok(ReconcileResult {
        channel_id: state_channel_id,
        candidate: Some(SettlementCandidate {
            channel_id: state.channel_id,
            instructions,
            kind: CandidateKind::Watermark,
            before: snapshot,
            after,
        }),
        store_disposition: StoreDisposition::Keep,
        snapshot,
        stablecoin_settled_base_units,
        stablecoin_distributed_base_units,
        escrow_active,
    })
}

fn channel_close_due(state: &ChannelState, now_ms: u64) -> bool {
    !state.sealed
        && state.open_slot.is_some()
        && (state.close_requested_at.is_some()
            || state
                .lifecycle
                .as_ref()
                .is_some_and(|lifecycle| lifecycle.close_after <= now_ms))
}

async fn observe_bound_close(
    runtime: &SettlementRuntime,
    store: &impl ChannelStore,
    state: ChannelState,
    config: &SessionConfig,
    now_seconds: u64,
) -> Result<ChannelState, JobError> {
    let channel_id = Pubkey::from_str(&state.channel_id)
        .map_err(|_| JobError::InvalidAddress(state.channel_id.clone()))?;
    let program = config
        .channel_program
        .unwrap_or_else(payment_channels::default_program_id);
    let Some(onchain) =
        channel::fetch_channel_for_program(&runtime.rpc, &runtime.rpc_url, &channel_id, &program)
            .await?
    else {
        return Ok(state);
    };
    verify_bound_channel(config, &state, &onchain)?;
    if !matches!(onchain.channel.status, STATUS_CLOSING | STATUS_SEALED) {
        return Ok(state);
    }
    if runtime.dry_run {
        let mut state = state;
        state.close_requested_at.get_or_insert(now_seconds);
        return Ok(state);
    }
    record_bound_close(store, &state.channel_id, config, onchain, now_seconds).await
}

async fn record_bound_close(
    store: &impl ChannelStore,
    channel_id: &str,
    config: &SessionConfig,
    onchain: channel::DecodedChannel,
    now_seconds: u64,
) -> Result<ChannelState, JobError> {
    let config = config.clone();
    store
        .update_channel(
            channel_id,
            Box::new(move |current| {
                let mut current =
                    current.ok_or_else(|| StoreError::Internal("Channel not found".into()))?;
                // Verify against the state read by the atomic update, not the
                // scan snapshot. Persist the external close even if a live
                // reservation prevents acquiring close ownership this tick.
                verify_bound_channel(&config, &current, &onchain)
                    .map_err(|error| StoreError::Internal(error.to_string()))?;
                current.close_requested_at.get_or_insert(now_seconds);
                Ok(current)
            }),
        )
        .await
        .map_err(|error| JobError::Config(format!("record on-chain close: {error}")))
}

async fn claim_due_close(
    store: &impl ChannelStore,
    channel_id: &str,
    now_ms: u64,
    now_seconds: u64,
    owner: &str,
    ttl: Duration,
) -> Result<ChannelState, JobError> {
    const MAX_ATTEMPTS: usize = 3;
    let mut last_error = None;
    for _ in 0..MAX_ATTEMPTS {
        let owner = owner.to_owned();
        let updated = store
            .update_channel(
                channel_id,
                Box::new(move |current| {
                    transition_due_close(current, now_ms, now_seconds, &owner, ttl)
                }),
            )
            .await;
        match updated {
            Ok(state) => return Ok(state),
            Err(error) => last_error = Some(error),
        }
    }
    Err(JobError::Config(format!(
        "claim session close after {MAX_ATTEMPTS} attempts: {}",
        last_error.expect("at least one claim attempt")
    )))
}

// Dry runs apply the durable claim rules to a snapshot without persisting
// ownership or starting a close heartbeat.
fn transition_due_close(
    current: Option<ChannelState>,
    now_ms: u64,
    now_seconds: u64,
    owner: &str,
    ttl: Duration,
) -> Result<ChannelState, StoreError> {
    let mut state = current.ok_or_else(|| StoreError::Internal("Channel not found".into()))?;
    if session_open_is_terminal(&state) {
        return Err(StoreError::Internal("session open is terminal".into()));
    }
    if let Some(value) = channel_binding(&state).map_err(|e| StoreError::Internal(e.to_string()))? {
        pay_types::deployment_policy::DeploymentSessionBinding::from_value(value)
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if !pay_types::deployment_policy::claim_close(
            &mut state,
            owner,
            now_ms,
            now_ms.saturating_add(ttl.as_millis() as u64),
        )
        .map_err(|e| StoreError::Internal(e.to_string()))?
        {
            return Err(StoreError::Internal(
                "bound close lease or reservation contended".into(),
            ));
        }
        Ok(state)
    } else {
        claim_channel_close(Some(state), now_ms, now_seconds)
    }
}

fn claim_channel_close(
    current: Option<ChannelState>,
    now_ms: u64,
    now_seconds: u64,
) -> Result<ChannelState, StoreError> {
    let mut state = current.ok_or_else(|| StoreError::Internal("Channel not found".to_string()))?;
    if channel_close_due(&state, now_ms) {
        state.close_requested_at.get_or_insert(now_seconds);
    }
    Ok(state)
}

#[allow(clippy::too_many_arguments)]
async fn build_idle_close_candidate(
    rpc: &RpcClient,
    rpc_url: &str,
    state: &ChannelState,
    onchain: &channel::DecodedChannel,
    now: i64,
    operator: &Pubkey,
    treasury_owner: &Pubkey,
    bound_config: Option<&SessionConfig>,
) -> Result<Option<SettlementCandidate>, JobError> {
    let program = bound_config
        .and_then(|config| config.channel_program)
        .unwrap_or_else(payment_channels::default_program_id);
    if onchain.payee() != *operator {
        return Err(JobError::Config(format!(
            "channel {} payee {} differs from lifecycle operator {operator}",
            state.channel_id,
            onchain.payee()
        )));
    }

    let Some(mut instructions) =
        close_settlement_instructions(state, onchain, now, operator, &program)?
    else {
        return Ok(None);
    };

    let (token_program, preimage) = if let Some(config) = bound_config {
        let token_program = config
            .token_program
            .ok_or_else(|| binding_error("missing token program"))?;
        channel::require_token_program(rpc, rpc_url, &onchain.mint(), &token_program).await?;
        (token_program, bound_preimage(config, onchain)?)
    } else {
        (
            channel::resolve_token_program(rpc, rpc_url, &onchain.mint()).await?,
            channel::recover_distribution_preimage(rpc, rpc_url, onchain).await?,
        )
    };
    instructions.push(
        channel::build_distribute_ix_for_program(
            onchain,
            treasury_owner,
            &token_program,
            &preimage,
            &program,
        )
        .0,
    );
    let before = inventory_snapshot(
        state.sealed,
        onchain.channel.status,
        onchain.channel.settlement.settled,
        onchain.channel.settlement.payout_watermark,
        state.cumulative,
        &onchain.mint(),
    );
    let after = inventory_snapshot(
        true,
        STATUS_DISTRIBUTED,
        onchain.channel.settlement.settled.max(state.cumulative),
        onchain.channel.settlement.settled.max(state.cumulative),
        state.cumulative,
        &onchain.mint(),
    );

    Ok(Some(SettlementCandidate {
        channel_id: state.channel_id.clone(),
        instructions,
        kind: CandidateKind::IdleClose,
        before,
        after,
    }))
}

fn close_settlement_instructions(
    state: &ChannelState,
    onchain: &channel::DecodedChannel,
    now: i64,
    operator: &Pubkey,
    program: &Pubkey,
) -> Result<Option<Vec<solana_instruction::Instruction>>, JobError> {
    let instructions = match onchain.channel.status {
        status
            if status == STATUS_OPEN
                || (status == STATUS_CLOSING && now < onchain.close_deadline()) =>
        {
            let onchain_signer = Pubkey::from(onchain.channel.authorized_signer.to_bytes());
            let (signature, cumulative, expires_at) = close_voucher(
                state,
                onchain.channel.settlement.settled,
                &onchain_signer,
                now,
            )?;
            payment_channels::build_settle_and_seal_instructions(
                operator,
                &onchain.address,
                &onchain_signer,
                signature.as_ref(),
                cumulative,
                expires_at,
                program,
            )
            .map_err(|error| JobError::TxBuild(format!("settle-and-seal instruction: {error}")))?
        }
        STATUS_SEALED => Vec::new(),
        STATUS_CLOSING if now >= onchain.close_deadline() => {
            vec![channel::build_seal_ix_for_program(
                &onchain.address,
                program,
            )]
        }
        STATUS_DISTRIBUTED => return Ok(None),
        status => {
            return Err(JobError::TxBuild(format!(
                "channel {} has unknown status {status}",
                state.channel_id
            )));
        }
    };

    Ok(Some(instructions))
}

fn close_voucher(
    state: &ChannelState,
    onchain_settled: u64,
    onchain_signer: &Pubkey,
    now: i64,
) -> Result<(Option<[u8; 64]>, u64, i64), JobError> {
    if state.cumulative <= onchain_settled {
        return Ok((None, onchain_settled, 0));
    }

    let authorized_signer = Pubkey::from_str(&state.authorized_signer)
        .map_err(|_| JobError::InvalidAddress(state.authorized_signer.clone()))?;
    if authorized_signer != *onchain_signer {
        return Err(JobError::Config(format!(
            "channel {} authorized signer differs from Redis",
            state.channel_id
        )));
    }
    let signature = state
        .highest_voucher_signature
        .as_deref()
        .ok_or_else(|| JobError::TxBuild("latest unsettled voucher has no signature".into()))?;
    let expires_at = state
        .highest_voucher_expires_at
        .ok_or_else(|| JobError::TxBuild("latest unsettled voucher has no expiry".into()))?;
    if expires_at != 0 && expires_at <= now {
        return Err(JobError::TxBuild(format!(
            "latest unsettled voucher for {} expired at {expires_at}",
            state.channel_id
        )));
    }
    let signature = decode_voucher_signature(signature)?;
    Ok((Some(signature), state.cumulative, expires_at))
}

fn decode_voucher_signature(signature: &str) -> Result<[u8; 64], JobError> {
    let mut out = [0u8; 64];
    five8::decode_64(signature, &mut out)
        .map_err(|error| JobError::TxBuild(format!("voucher signature: {error}")))?;
    Ok(out)
}

struct SettlementLock {
    connection: redis::aio::ConnectionManager,
    key: String,
    owner: String,
    heartbeat: LeaseHeartbeat,
}

impl SettlementLock {
    async fn acquire(
        redis_url: &str,
        lock_key: &str,
        ttl_seconds: u64,
    ) -> Result<Option<Self>, JobError> {
        let client = redis::Client::open(redis_url)
            .map_err(|error| JobError::Config(format!("Redis client: {error}")))?;
        let mut connection = client
            .get_connection_manager()
            .await
            .map_err(|error| JobError::Config(format!("Redis connect: {error}")))?;
        let owner = format!("{}-{}", std::process::id(), unix_nanos());
        let acquired: Option<String> = redis::cmd("SET")
            .arg(lock_key)
            .arg(&owner)
            .arg("NX")
            .arg("EX")
            .arg(ttl_seconds.max(1))
            .query_async(&mut connection)
            .await
            .map_err(|error| JobError::Config(format!("Redis settlement lock: {error}")))?;
        Ok(acquired.map(|_| Self {
            heartbeat: LeaseHeartbeat::start(
                connection.clone(),
                lock_key.to_string(),
                owner.clone(),
                ttl_seconds,
            ),
            connection,
            key: lock_key.to_string(),
            owner,
        }))
    }

    async fn release(mut self) {
        self.heartbeat.shutdown().await;
        const RELEASE: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
"#;
        if let Err(error) = redis::Script::new(RELEASE)
            .key(&self.key)
            .arg(&self.owner)
            .invoke_async::<i32>(&mut self.connection)
            .await
        {
            warn!(%error, "failed to release settlement lease; TTL will expire it");
        }
    }
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_bool_env(name: &str, default: bool) -> Result<bool, JobError> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| JobError::Config(format!("{name} must be true or false"))),
        Err(_) => Ok(default),
    }
}

fn parse_u64_env(name: &str, default: u64) -> Result<u64, JobError> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| JobError::Config(format!("{name} must be an integer"))),
        Err(_) => Ok(default),
    }
}

fn require_positive_u64(name: &str, value: u64) -> Result<u64, JobError> {
    if value == 0 {
        return Err(JobError::Config(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(value)
}

fn parse_optional_positive_u64_env(name: &str) -> Result<Option<u64>, JobError> {
    let Some(value) = optional_env(name) else {
        return Ok(None);
    };
    let value = value
        .parse::<u64>()
        .map_err(|_| JobError::Config(format!("{name} must be an integer")))?;
    if value == 0 {
        return Err(JobError::Config(format!(
            "{name} must be greater than zero when set"
        )));
    }
    Ok(Some(value))
}

fn distribution_threshold_reached(
    target_settled: u64,
    distributed: u64,
    threshold_base_units: u64,
) -> bool {
    target_settled.saturating_sub(distributed) >= threshold_base_units
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn unix_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_kit::core::store::ChannelLifecycle;

    fn channel_state() -> ChannelState {
        ChannelState {
            channel_id: Pubkey::new_unique().to_string(),
            authorized_signer: Pubkey::new_unique().to_string(),
            deposit: 1_000_000,
            cumulative: 0,
            sealed: false,
            highest_voucher_signature: None,
            highest_voucher_expires_at: None,
            close_requested_at: None,
            final_cumulative: None,
            open_slot: Some(42),
            payer: Pubkey::new_unique().to_string(),
            rent_payer: Pubkey::new_unique().to_string(),
            opening_challenge_id: String::new(),
            authentication: None,
            voucher_signer: "client".to_string(),
            idle_timeout_seconds: None,
            last_activity_at: 0,
            spent_amount: 0,
            settled_on_chain: 0,
            distributed_on_chain: 0,
            processed_uses: vec![],
            processed_topup_signatures: vec![],
            next_delivery_sequence: 0,
            pending_deliveries: vec![],
            committed_deliveries: Default::default(),
            pending_setup: None,
            onchain_checked_at: 0,
            lifecycle: None,
            schema_version: pay_kit::mpp::CHANNEL_STATE_SCHEMA_VERSION,
            extra: Default::default(),
        }
    }

    fn bound_fixture() -> (ChannelState, SessionConfig) {
        use pay_types::deployment_policy::{
            DeploymentIdentity, DeploymentSessionBinding, DurableDeploymentPolicy, PolicyAllocation,
        };
        let operator = Pubkey::new_unique();
        let binding = DeploymentSessionBinding::new(DurableDeploymentPolicy {
            deployment: DeploymentIdentity {
                owner_key: "owner".into(),
                resource_name: "service".into(),
                created_at: "incarnation-1".into(),
                hostname: "service.example.com".into(),
            },
            version: 1,
            price_micro_usd: 50_000,
            allocations: [(30_000, 6000), (15_000, 3000), (5000, 1000)]
                .into_iter()
                .map(|(amount_micro_usd, basis_points)| PolicyAllocation {
                    recipient: Pubkey::new_unique().to_string(),
                    amount_micro_usd,
                    basis_points,
                })
                .collect(),
        })
        .unwrap();
        let payout = binding.effective_payout(&operator.to_string()).unwrap();
        let config = SessionConfig {
            operator: operator.to_string(),
            recipient: payout.recipient,
            amount: binding.policy.price_micro_usd,
            network: "devnet".into(),
            splits: payout
                .splits
                .into_iter()
                .map(|split| pay_kit::mpp::server::session::Split {
                    recipient: split.recipient.parse().unwrap(),
                    bps: split.bps,
                })
                .collect(),
            ..SessionConfig::default()
        };
        let mut state = channel_state();
        state.lifecycle = Some(ChannelLifecycle {
            owner: "proxy".into(),
            close_after: 1000,
        });
        state.extra.insert(
            "mppSessionBinding".into(),
            serde_json::json!({
                "version": 1,
                "policy": binding.to_value().unwrap(),
                "snapshot": SessionConfigSnapshot::capture(&config),
            }),
        );
        (state, config)
    }

    #[test]
    fn restart_restores_original_terms_without_current_deployment() {
        let (state, original) = bound_fixture();
        let restored: ChannelState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        let binding = pay_types::deployment_policy::DeploymentSessionBinding::from_value(
            channel_binding(&restored).unwrap().unwrap(),
        )
        .unwrap();
        let mut config = SessionConfig {
            recipient: Pubkey::new_unique().to_string(),
            amount: 999_999,
            network: "mainnet".into(),
            ..SessionConfig::default()
        };
        SessionConfigSnapshot::from_channel(&restored)
            .unwrap()
            .unwrap()
            .apply_to(&mut config)
            .unwrap();
        validate_bound_config(
            &binding,
            &config,
            "devnet",
            &original.operator.parse().unwrap(),
        )
        .unwrap();
        assert_eq!(config.amount, 50_000);
        assert_eq!(config.recipient, original.recipient);
        assert_eq!(
            config
                .splits
                .iter()
                .map(|split| split.bps)
                .collect::<Vec<_>>(),
            vec![3000, 1000, 6000]
        );
        let server = SessionServer::new(config, pay_kit::core::store::MemoryChannelStore::new())
            .with_channel_binding(binding.to_value().unwrap());
        server.require_channel_binding(&restored).unwrap();
    }

    #[test]
    fn fleet_reclamation_requires_recorded_legacy_ownership() {
        assert!(!channel::is_recorded_legacy(None).unwrap());
        assert!(channel::is_recorded_legacy(Some(&channel_state())).unwrap());
        let (mut state, _) = bound_fixture();
        assert!(!channel::is_recorded_legacy(Some(&state)).unwrap());
        state
            .extra
            .insert("mppSessionBinding".into(), serde_json::Value::Null);
        assert!(channel::is_recorded_legacy(Some(&state)).is_err());
    }

    #[tokio::test]
    async fn durable_scan_resumes_pending_open_without_a_client_retry() {
        use pay_kit::core::store::{MemoryChannelStore, PendingSetup};
        let (mut state, _) = bound_fixture();
        state.pending_setup = Some(PendingSetup {
            payer_signature: "persisted-validated-signature".into(),
            deposit: 50_000,
            opens_channel: true,
            expires_at: 1,
        });
        state.deposit = 0;
        let store = MemoryChannelStore::new();
        store
            .put_channel(&state.channel_id, state.clone())
            .await
            .unwrap();
        let scanned = store.list_channels().await.unwrap().pop().unwrap();
        // The verifier is mocked here; kit tests cover actual signed-intent
        // read-back and expired-challenge recovery. This tests worker selection.
        let resumed = resume_scanned_pending_open(&scanned, false, |id| {
            let store = &store;
            async move {
                store
                    .update_channel(
                        &id,
                        Box::new(|current| {
                            let mut state = current.unwrap();
                            state.deposit = state.pending_setup.take().unwrap().deposit;
                            Ok(state)
                        }),
                    )
                    .await
                    .map_err(binding_error)?;
                Ok(())
            }
        })
        .await
        .unwrap();
        assert!(resumed);
        let recovered = store.get_channel(&state.channel_id).await.unwrap().unwrap();
        assert_eq!(recovered.deposit, 50_000);
        assert_eq!(
            channel_binding(&recovered).unwrap(),
            channel_binding(&state).unwrap()
        );
        assert!(
            !resume_scanned_pending_open(&recovered, false, |_| async {
                panic!("active channel must not be reopened");
            })
            .await
            .unwrap()
        );
        assert!(
            resume_scanned_pending_open(&scanned, true, |_| async {
                panic!("dry run must not broadcast recovery");
            })
            .await
            .is_err()
        );
        assert!(
            resume_scanned_pending_open(&scanned, false, |_| async {
                Err(binding_error("uncertain confirmation"))
            })
            .await
            .is_err()
        );
        state.pending_setup.as_mut().unwrap().opens_channel = false;
        assert!(
            !resume_scanned_pending_open(&state, false, |_| async {
                panic!("top-up is not a pending open");
            })
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn unknown_bindings_and_unrelated_channels_are_not_adopted() {
        let (mut state, config) = bound_fixture();
        let store = Arc::new(pay_kit::core::store::MemoryChannelStore::new());
        let binding = channel_binding(&state).unwrap().unwrap();
        let server = SessionServer::new(config, store.clone()).with_channel_binding(binding);
        assert!(
            server
                .resume_pending_open(&Pubkey::new_unique().to_string())
                .await
                .is_err()
        );
        assert!(store.list_channels().await.unwrap().is_empty());
        assert!(server.require_channel_binding(&channel_state()).is_err());
        state.extra.get_mut("mppSessionBinding").unwrap()["version"] = serde_json::json!(99);
        assert!(channel_binding(&state).is_err());
        assert!(SessionConfigSnapshot::from_channel(&state).is_err());
        assert!(server.require_channel_binding(&state).is_err());
        store
            .put_channel(&state.channel_id, state.clone())
            .await
            .unwrap();
        assert!(
            claim_due_close(
                &store,
                &state.channel_id,
                1000,
                1,
                "worker",
                Duration::from_secs(1)
            )
            .await
            .is_err()
        );
        assert!(
            store
                .get_channel(&state.channel_id)
                .await
                .unwrap()
                .unwrap()
                .close_requested_at
                .is_none()
        );
    }

    #[test]
    fn bound_distribution_instruction_preserves_exact_ordered_payout_and_program() {
        use pay_kit::generated::payment_channels::generated::{
            accounts::Channel, types::SettlementWatermarks,
        };
        let (state, original) = bound_fixture();
        let mut config = SessionConfig::default();
        SessionConfigSnapshot::capture(&original)
            .apply_to(&mut config)
            .unwrap();
        // The instruction must use the stored program, never the fleet default.
        config.channel_program = Some(Pubkey::new_unique());
        let recipients: Vec<_> = config
            .splits
            .iter()
            .map(|split| payment_channels::Distribution {
                recipient: split.recipient,
                bps: split.bps,
            })
            .collect();
        let address = |value: &str| payment_channels::to_address(&value.parse().unwrap());
        let mut onchain = channel::DecodedChannel {
            address: state.channel_id.parse().unwrap(),
            channel: Channel {
                discriminator: 1,
                version: 1,
                bump: 1,
                status: STATUS_SEALED,
                salt: 0,
                deposit: 50_000,
                settlement: SettlementWatermarks {
                    settled: 50_000,
                    payout_watermark: 0,
                },
                closure_started_at: 0,
                payer_withdrawn_at: 0,
                grace_period: config.grace_period_seconds,
                distribution_hash: payment_channels::distribution_hash(&recipients),
                payer: address(&state.payer),
                payee: address(&config.recipient),
                authorized_signer: address(&state.authorized_signer),
                mint: address(&config.currency),
                rent_payer: address(&state.rent_payer),
                open_slot: state.open_slot.unwrap(),
            },
        };
        verify_bound_channel(&config, &state, &onchain).unwrap();
        let preimage = bound_preimage(&config, &onchain).unwrap();
        assert_eq!(
            preimage
                .recipients
                .iter()
                .map(|split| 50_000_u64 * u64::from(split.bps) / 10_000)
                .collect::<Vec<_>>(),
            vec![15_000, 5000, 30_000]
        );
        let (instruction, accounts) = channel::build_distribute_ix_for_program(
            &onchain,
            &Pubkey::new_unique(),
            &config.token_program.unwrap(),
            &preimage,
            &config.channel_program.unwrap(),
        );
        assert_eq!(
            instruction.program_id.to_bytes(),
            config.channel_program.unwrap().to_bytes()
        );
        assert!(instruction.data.ends_with(&preimage.preimage_bytes));
        for (split, ata) in config.splits.iter().zip(accounts.recipient_atas) {
            assert_eq!(
                ata,
                pay_api_core::ata::associated_token_address(
                    &split.recipient,
                    &onchain.mint(),
                    &config.token_program.unwrap(),
                )
            );
            assert!(
                instruction
                    .accounts
                    .iter()
                    .any(|account| account.pubkey.to_bytes() == ata.to_bytes())
            );
        }
        onchain.channel.distribution_hash[0] ^= 1;
        assert!(verify_bound_channel(&config, &state, &onchain).is_err());
        onchain.channel.distribution_hash = payment_channels::distribution_hash(&recipients);
        onchain.channel.payee = payment_channels::to_address(&Pubkey::new_unique());
        assert!(verify_bound_channel(&config, &state, &onchain).is_err());
    }

    fn closing_fixture() -> (ChannelState, SessionConfig, channel::DecodedChannel) {
        use pay_kit::generated::payment_channels::generated::{
            accounts::Channel, types::SettlementWatermarks,
        };
        let (mut state, mut config) = bound_fixture();
        SessionConfigSnapshot::capture(&config)
            .apply_to(&mut config)
            .unwrap();
        state.lifecycle.as_mut().unwrap().close_after = 1_000_000;
        state.cumulative = 50_000;
        state.highest_voucher_signature = Some(bs58::encode([42_u8; 64]).into_string());
        state.highest_voucher_expires_at = Some(500);
        let recipients = config
            .splits
            .iter()
            .map(|split| payment_channels::Distribution {
                recipient: split.recipient,
                bps: split.bps,
            })
            .collect::<Vec<_>>();
        let address = |value: &str| payment_channels::to_address(&value.parse().unwrap());
        let onchain = channel::DecodedChannel {
            address: state.channel_id.parse().unwrap(),
            channel: Channel {
                discriminator: 1,
                version: 1,
                bump: 1,
                status: STATUS_CLOSING,
                salt: 0,
                deposit: 100_000,
                settlement: SettlementWatermarks {
                    settled: 10_000,
                    payout_watermark: 0,
                },
                closure_started_at: 100,
                payer_withdrawn_at: 0,
                grace_period: 100,
                distribution_hash: payment_channels::distribution_hash(&recipients),
                payer: address(&state.payer),
                payee: address(&config.recipient),
                authorized_signer: address(&state.authorized_signer),
                mint: address(&config.currency),
                rent_payer: address(&state.rent_payer),
                open_slot: state.open_slot.unwrap(),
            },
        };
        (state, config, onchain)
    }

    #[tokio::test]
    async fn dry_run_due_open_plans_close_without_persisting() {
        let (mut state, config, mut onchain) = closing_fixture();
        state.lifecycle.as_mut().unwrap().close_after = 120_000;
        onchain.channel.status = STATUS_OPEN;
        onchain.channel.closure_started_at = 0;
        assert!(state.close_requested_at.is_none());
        assert!(channel_close_due(&state, 120_000));
        let store = pay_kit::core::store::MemoryChannelStore::new();
        let id = state.channel_id.clone();
        store.put_channel(&id, state.clone()).await.unwrap();
        let planned = transition_due_close(
            Some(state.clone()),
            120_000,
            120,
            "worker",
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(planned.close_requested_at, Some(120));
        verify_bound_channel(&config, &planned, &onchain).unwrap();
        assert!(
            close_settlement_instructions(
                &planned,
                &onchain,
                120,
                &config.operator.parse().unwrap(),
                &config.channel_program.unwrap(),
            )
            .unwrap()
            .is_some()
        );
        assert_eq!(
            serde_json::to_value(store.get_channel(&id).await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(state).unwrap(),
        );
    }

    #[tokio::test]
    async fn dry_run_due_open_with_active_reservation_remains_unplanned() {
        let (mut state, _, _) = closing_fixture();
        pay_types::deployment_policy::reserve(&mut state, "request", 1, 120_000, 130_000).unwrap();
        state.lifecycle.as_mut().unwrap().close_after = 120_000;
        assert!(state.close_requested_at.is_none());
        let store = pay_kit::core::store::MemoryChannelStore::new();
        let id = state.channel_id.clone();
        store.put_channel(&id, state.clone()).await.unwrap();
        assert!(
            transition_due_close(
                Some(state.clone()),
                120_000,
                120,
                "worker",
                Duration::from_secs(10),
            )
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(store.get_channel(&id).await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(state).unwrap(),
        );
    }

    #[tokio::test]
    async fn external_close_before_idle_deadline_settles_latest_voucher_in_grace() {
        let (state, config, onchain) = closing_fixture();
        let store = pay_kit::core::store::MemoryChannelStore::new();
        let id = state.channel_id.clone();
        assert!(!channel_close_due(&state, 120_000));
        store.put_channel(&id, state).await.unwrap();
        // A voucher committed after the scan must be carried by the close.
        store
            .update_channel(
                &id,
                Box::new(|current| {
                    let mut state = current.unwrap();
                    state.cumulative = 60_000;
                    state.highest_voucher_signature = Some(bs58::encode([43_u8; 64]).into_string());
                    Ok(state)
                }),
            )
            .await
            .unwrap();
        record_bound_close(&store, &id, &config, onchain.clone(), 120)
            .await
            .unwrap();
        let state = claim_due_close(&store, &id, 120_000, 120, "worker", Duration::from_secs(10))
            .await
            .unwrap();
        let program = config.channel_program.unwrap();
        let actual = close_settlement_instructions(
            &state,
            &onchain,
            120,
            &config.operator.parse().unwrap(),
            &program,
        )
        .unwrap()
        .unwrap();
        let expected = payment_channels::build_settle_and_seal_instructions(
            &config.operator.parse().unwrap(),
            &onchain.address,
            &state.authorized_signer.parse().unwrap(),
            Some(&[43_u8; 64]),
            60_000,
            500,
            &program,
        )
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(state.close_requested_at, Some(120));
    }

    #[tokio::test]
    async fn external_close_preserves_marker_while_reservation_defers_and_expiry_allows_takeover() {
        let (mut state, config, onchain) = closing_fixture();
        pay_types::deployment_policy::reserve(&mut state, "request", 1, 120_000, 130_000).unwrap();
        let store = pay_kit::core::store::MemoryChannelStore::new();
        let id = state.channel_id.clone();
        store.put_channel(&id, state).await.unwrap();
        record_bound_close(&store, &id, &config, onchain, 120)
            .await
            .unwrap();
        assert!(
            claim_due_close(&store, &id, 120_000, 120, "a", Duration::from_secs(10))
                .await
                .is_err()
        );
        assert_eq!(
            store
                .get_channel(&id)
                .await
                .unwrap()
                .unwrap()
                .close_requested_at,
            Some(120)
        );
        claim_due_close(&store, &id, 130_000, 130, "a", Duration::from_secs(10))
            .await
            .unwrap();
        assert!(
            claim_due_close(&store, &id, 130_001, 130, "b", Duration::from_secs(10))
                .await
                .is_err()
        );
        claim_due_close(&store, &id, 140_000, 140, "b", Duration::from_secs(10))
            .await
            .unwrap();
        assert!(
            claim_due_close(&store, &id, 140_001, 140, "a", Duration::from_secs(10))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn external_close_wrong_identity_never_writes_marker() {
        let (state, config, onchain) = closing_fixture();
        let store = pay_kit::core::store::MemoryChannelStore::new();
        let id = state.channel_id.clone();
        store.put_channel(&id, state).await.unwrap();
        for mutation in 0..3 {
            let mut wrong = onchain.clone();
            match mutation {
                0 => wrong.channel.payer = payment_channels::to_address(&Pubkey::new_unique()),
                1 => wrong.channel.distribution_hash[0] ^= 1,
                _ => wrong.address = Pubkey::new_unique(),
            }
            assert!(
                record_bound_close(&store, &id, &config, wrong, 120)
                    .await
                    .is_err()
            );
        }
        assert!(
            store
                .get_channel(&id)
                .await
                .unwrap()
                .unwrap()
                .close_requested_at
                .is_none()
        );
    }

    #[tokio::test]
    async fn external_sealed_close_and_terminal_tombstone_fail_closed() {
        let (mut state, config, mut onchain) = closing_fixture();
        onchain.channel.status = STATUS_SEALED;
        let store = pay_kit::core::store::MemoryChannelStore::new();
        let id = state.channel_id.clone();
        store.put_channel(&id, state.clone()).await.unwrap();
        let marked = record_bound_close(&store, &id, &config, onchain.clone(), 120)
            .await
            .unwrap();
        assert_eq!(marked.close_requested_at, Some(120));
        state = marked;
        state
            .extra
            .insert("mppSessionOpenTerminal".into(), serde_json::Value::Null);
        let terminal = state.clone();
        store
            .update_channel(&id, Box::new(move |_| Ok(terminal)))
            .await
            .unwrap();
        assert!(session_open_is_terminal(&state));
        assert!(
            record_bound_close(&store, &id, &config, onchain, 120)
                .await
                .is_err()
        );
        assert!(
            claim_due_close(&store, &id, 120_000, 120, "a", Duration::from_secs(10))
                .await
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(store.get_channel(&id).await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(state).unwrap(),
        );
    }

    #[tokio::test]
    async fn external_close_waits_for_pending_setup_and_authorization() {
        use pay_kit::core::store::{PendingDelivery, PendingSetup};
        let (mut state, config, onchain) = closing_fixture();
        state.pending_setup = Some(PendingSetup {
            payer_signature: "pending".into(),
            deposit: 1,
            opens_channel: false,
            expires_at: 999,
        });
        let store = pay_kit::core::store::MemoryChannelStore::new();
        let id = state.channel_id.clone();
        store.put_channel(&id, state).await.unwrap();
        record_bound_close(&store, &id, &config, onchain, 120)
            .await
            .unwrap();
        assert!(
            claim_due_close(&store, &id, 120_000, 120, "a", Duration::from_secs(10))
                .await
                .is_err()
        );
        assert_eq!(
            store
                .get_channel(&id)
                .await
                .unwrap()
                .unwrap()
                .close_requested_at,
            Some(120)
        );
        store
            .update_channel(
                &id,
                Box::new(|current| {
                    let mut state = current.unwrap();
                    state.pending_setup = None;
                    state.pending_deliveries.push(PendingDelivery {
                        delivery_id: "pending-delivery".into(),
                        amount: 1,
                        sequence: 0,
                        expires_at: 130,
                        request_fingerprint: None,
                        handler_succeeded: false,
                    });
                    Ok(state)
                }),
            )
            .await
            .unwrap();
        assert!(
            claim_due_close(&store, &id, 120_000, 120, "a", Duration::from_secs(10))
                .await
                .is_err()
        );
        claim_due_close(&store, &id, 130_000, 130, "a", Duration::from_secs(10))
            .await
            .unwrap();
    }

    async fn assert_reservation_close_exclusion(store: &impl ChannelStore) {
        for _ in 0..20 {
            let (state, _) = bound_fixture();
            let id = state.channel_id.clone();
            store.put_channel(&id, state).await.unwrap();
            let (closed, reserved) = tokio::join!(
                claim_due_close(store, &id, 1000, 1, "worker-a", Duration::from_secs(1)),
                store.update_channel(
                    &id,
                    Box::new(|current| {
                        let mut state = current.unwrap();
                        pay_types::deployment_policy::reserve(
                            &mut state, "request", 50_000, 1000, 2000,
                        )
                        .map_err(|error| StoreError::Internal(error.to_string()))?;
                        Ok(state)
                    })
                ),
            );
            assert_ne!(
                closed.is_ok(),
                reserved.is_ok(),
                "only one CAS contender may own lifecycle"
            );
            // A crash/expired reservation is recoverable; a live close owner
            // excludes another external job even when the global job lock differs.
            let recovered =
                claim_due_close(store, &id, 2001, 2, "worker-b", Duration::from_secs(1))
                    .await
                    .unwrap();
            assert!(recovered.close_requested_at.is_some());
            assert!(
                claim_due_close(store, &id, 2002, 2, "worker-c", Duration::from_secs(1))
                    .await
                    .is_err()
            );
            claim_due_close(store, &id, 2500, 2, "worker-b", Duration::from_secs(1))
                .await
                .unwrap();
            assert!(
                claim_due_close(store, &id, 3002, 3, "worker-c", Duration::from_secs(1))
                    .await
                    .is_err()
            );
            claim_due_close(store, &id, 3501, 3, "worker-c", Duration::from_secs(1))
                .await
                .unwrap();
            store.delete_channel(&id).await.unwrap();
        }
    }

    #[tokio::test]
    async fn concurrent_reservation_and_close_use_one_durable_transition() {
        assert_reservation_close_exclusion(&pay_kit::core::store::MemoryChannelStore::new()).await;
    }

    #[tokio::test]
    #[ignore = "requires disposable PAY_WORKER_TEST_REDIS_URL"]
    async fn redis_cas_reservation_close_and_external_job_retries() {
        let url = std::env::var("PAY_WORKER_TEST_REDIS_URL").unwrap();
        let store = RedisChannelStore::connect_with_finalized_retention(
            &url,
            format!("pay-worker-test:{}:", unix_nanos()),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_reservation_close_exclusion(&store).await;
    }

    #[test]
    fn resource_cleanup_is_due_when_channel_is_spent_or_terminal() {
        let mut state = channel_state();
        assert!(!resource_cleanup_required(&state, 1_000));

        state.deposit = 10;
        state.cumulative = 10;
        state.lifecycle = Some(ChannelLifecycle {
            owner: "worker".into(),
            close_after: 1_250,
        });
        assert!(!resource_cleanup_required(&state, 1_000));
        assert!(resource_cleanup_required(&state, 1_250));

        state.extra.insert(
            RESOURCE_CLEANUP_COMPLETED_AT.into(),
            serde_json::Value::from(1),
        );
        assert!(resource_cleanup_required(&state, 1_250));

        state.extra.clear();
        state.cumulative = 0;
        state.close_requested_at = Some(1);
        assert!(resource_cleanup_required(&state, 1_000));
    }

    #[test]
    fn resource_cleanup_requires_a_quiet_window_and_resets_when_resources_reappear() {
        let mut state = channel_state();
        assert!(!record_resource_cleanup(&mut state, 1_000, 500, 0));
        assert!(!record_resource_cleanup(&mut state, 1_400, 500, 0));

        assert!(!record_resource_cleanup(&mut state, 1_500, 500, 1));
        assert!(cleanup_timestamp(&state, RESOURCE_CLEANUP_EMPTY_SINCE).is_none());

        assert!(!record_resource_cleanup(&mut state, 2_000, 500, 0));
        assert!(record_resource_cleanup(&mut state, 2_500, 500, 0));
    }

    #[test]
    fn snapshot_max_age_must_be_positive() {
        let error = require_positive_u64("PAY_X402_SNAPSHOT_MAX_AGE_SECS", 0).unwrap_err();
        assert_eq!(
            error.to_string(),
            "config error: PAY_X402_SNAPSHOT_MAX_AGE_SECS must be greater than zero"
        );
        assert_eq!(
            require_positive_u64("PAY_X402_SNAPSHOT_MAX_AGE_SECS", 30).unwrap(),
            30
        );
    }

    #[test]
    fn batch_reconciliation_sweeps_every_positive_watermark() {
        let mut state = channel_state();
        state.cumulative = 1;
        state.last_activity_at = 999;
        assert!(!batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(2_000),
            false,
            None,
        ));
        assert!(batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(2_000),
            true,
            None,
        ));

        state.settled_on_chain = 1;
        assert!(!batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(2_000),
            true,
            None,
        ));
    }

    #[test]
    fn batch_reconciliation_flushes_idle_residuals_and_lifecycle_work() {
        let mut state = channel_state();
        state.cumulative = 1;
        state.last_activity_at = 699;
        assert!(batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(2_000),
            false,
            None,
        ));

        state.cumulative = 0;
        state.close_requested_at = Some(1);
        assert!(batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(2_000),
            false,
            None,
        ));
    }

    #[test]
    fn batch_reconciliation_includes_threshold_distribution_without_a_new_claim() {
        let mut state = channel_state();
        state.cumulative = 2_000;
        state.settled_on_chain = 2_000;
        state.distributed_on_chain = 999;

        assert!(batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(2_000),
            false,
            Some(1_000),
        ));
        state.distributed_on_chain = 1_001;
        assert!(!batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(2_000),
            false,
            Some(1_000),
        ));
    }

    #[test]
    fn batch_reconciliation_refreshes_stale_fully_settled_channels() {
        let mut state = channel_state();
        state.cumulative = 1_000;
        state.settled_on_chain = 1_000;
        state.onchain_checked_at = 969;

        assert!(batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(30),
            false,
            None,
        ));

        state.onchain_checked_at = 971;
        assert!(!batch_reconciliation_due(
            &state,
            1_000,
            Duration::from_secs(300),
            Duration::from_secs(30),
            false,
            None,
        ));
    }

    #[test]
    fn batch_distribution_threshold_uses_only_the_undistributed_delta() {
        assert!(!distribution_threshold_reached(999, 0, 1_000));
        assert!(distribution_threshold_reached(1_000, 0, 1_000));
        assert!(!distribution_threshold_reached(1_500, 501, 1_000));
        assert!(distribution_threshold_reached(1_501, 501, 1_000));
    }

    #[test]
    fn usdtest_is_included_in_worker_stablecoin_metrics() {
        assert_eq!(stablecoin_base_units(&DEVNET_USDTEST_MINT, 42), 42);
    }

    #[test]
    fn inventory_classifies_channel_lifecycle_state() {
        let empty_open =
            inventory_snapshot(false, STATUS_OPEN, 0, 0, 123, &MAINNET_STABLECOIN_MINT);
        assert!(empty_open.opened_zero_settlements);
        assert!(empty_open.unsealed);
        assert!(!empty_open.rent_unclaimed);
        assert_eq!(empty_open.stablecoin_settled_base_units, 0);
        assert_eq!(empty_open.stablecoin_undistributed_base_units, 0);
        assert_eq!(empty_open.stablecoin_distributed_base_units, 0);
        assert_eq!(empty_open.stablecoin_unsettled_base_units, 123);

        let stuck = inventory_snapshot(
            true,
            STATUS_OPEN,
            17_581,
            4_000,
            17_581,
            &MAINNET_STABLECOIN_MINT,
        );
        assert!(stuck.unsealed);
        assert!(stuck.redis_chain_mismatch);
        assert_eq!(stuck.stablecoin_settled_base_units, 17_581);
        assert_eq!(stuck.stablecoin_undistributed_base_units, 13_581);
        assert_eq!(stuck.stablecoin_distributed_base_units, 4_000);
        assert_eq!(stuck.stablecoin_unsettled_base_units, 0);

        let distributed = inventory_snapshot(
            true,
            STATUS_DISTRIBUTED,
            17_581,
            0,
            17_581,
            &MAINNET_STABLECOIN_MINT,
        );
        assert!(distributed.rent_unclaimed);
        assert!(!distributed.unsealed);
        assert!(!distributed.redis_chain_mismatch);
        assert_eq!(distributed.stablecoin_settled_base_units, 17_581);
        assert_eq!(distributed.stablecoin_undistributed_base_units, 0);
        assert_eq!(distributed.stablecoin_distributed_base_units, 17_581);
    }

    #[test]
    fn terminal_status_supersedes_the_final_payout_watermark() {
        assert_eq!(
            effective_distributed_amount(STATUS_DISTRIBUTED, 17_581, 0),
            17_581
        );
        assert_eq!(
            effective_distributed_amount(STATUS_OPEN, 17_581, 4_000),
            4_000
        );
    }

    #[test]
    fn unsettled_stablecoin_uses_cumulative_voucher_delta_in_base_units() {
        assert_eq!(
            unsettled_stablecoin_base_units(&MAINNET_STABLECOIN_MINT, 1_234_567, 234_567),
            1_000_000
        );
        assert_eq!(
            unsettled_stablecoin_base_units(&DEVNET_STABLECOIN_MINT, 100, 101),
            0
        );
        assert_eq!(
            unsettled_stablecoin_base_units(&Pubkey::new_unique(), 1_234_567, 0),
            0
        );
        assert_eq!(
            settled_stablecoin_base_units(&MAINNET_STABLECOIN_MINT, 1_234_567),
            1_234_567
        );
        assert_eq!(
            settled_stablecoin_base_units(&Pubkey::new_unique(), 1_234_567),
            0
        );
    }

    #[test]
    fn confirmed_settlement_updates_the_reported_inventory_snapshot() {
        let before = inventory_snapshot(
            false,
            STATUS_OPEN,
            100_000,
            25_000,
            150_000,
            &MAINNET_STABLECOIN_MINT,
        );
        let after = inventory_snapshot(
            false,
            STATUS_OPEN,
            150_000,
            25_000,
            150_000,
            &MAINNET_STABLECOIN_MINT,
        );
        let mut inventory = LifecycleInventory::default();

        inventory.record(before);
        inventory.replace(before, after);

        assert_eq!(inventory.stablecoin_settled_base_units, 150_000);
        assert_eq!(inventory.stablecoin_undistributed_base_units, 125_000);
        assert_eq!(inventory.stablecoin_distributed_base_units, 25_000);
        assert_eq!(inventory.stablecoin_unsettled_base_units, 0);
        assert_eq!(inventory.unsealed, 1);
        assert_eq!(inventory.rent_unclaimed, 0);
    }

    #[test]
    fn absent_push_channel_is_deleted_but_pull_session_is_kept() {
        let push = channel_state();
        assert_eq!(
            absent_onchain_store_disposition(&push),
            StoreDisposition::Delete
        );

        let mut pull = channel_state();
        pull.open_slot = None;
        assert_eq!(
            absent_onchain_store_disposition(&pull),
            StoreDisposition::Keep
        );
    }

    #[test]
    fn idle_close_requires_a_due_push_channel() {
        let mut state = channel_state();
        state.lifecycle = Some(ChannelLifecycle {
            owner: "proxy-a".to_string(),
            close_after: 120_000,
        });

        assert!(!channel_close_due(&state, 119_999));
        assert!(channel_close_due(&state, 120_000));

        state.lifecycle.as_mut().unwrap().close_after = 180_000;
        state.close_requested_at = Some(120);
        assert!(
            channel_close_due(&state, 120_000),
            "a previously claimed close must resume on the next worker run"
        );

        state.sealed = true;
        assert!(!channel_close_due(&state, 120_000));

        state.sealed = false;
        state.open_slot = None;
        assert!(
            !channel_close_due(&state, 120_000),
            "pull sessions do not have payment channels to close"
        );
    }

    #[test]
    fn close_claim_rechecks_the_latest_deadline() {
        let mut state = channel_state();
        state.lifecycle = Some(ChannelLifecycle {
            owner: "proxy-a".to_string(),
            close_after: 180_000,
        });

        let unchanged = claim_channel_close(Some(state.clone()), 120_000, 120).unwrap();
        assert!(
            unchanged.close_requested_at.is_none(),
            "a concurrent wake-up must cancel the stale close candidate"
        );

        state.lifecycle.as_mut().unwrap().close_after = 120_000;
        let claimed = claim_channel_close(Some(state), 120_000, 120).unwrap();
        assert_eq!(claimed.close_requested_at, Some(120));
    }

    #[test]
    fn idle_close_uses_only_a_strictly_newer_unexpired_voucher() {
        let signer = Pubkey::new_unique();
        let mut state = channel_state();
        state.authorized_signer = signer.to_string();
        state.cumulative = 75;
        state.highest_voucher_signature = Some(bs58::encode([7_u8; 64]).into_string());
        state.highest_voucher_expires_at = Some(500);

        let (signature, cumulative, expires_at) = close_voucher(&state, 50, &signer, 400).unwrap();
        assert_eq!(signature, Some([7_u8; 64]));
        assert_eq!(cumulative, 75);
        assert_eq!(expires_at, 500);

        let (signature, cumulative, expires_at) = close_voucher(&state, 75, &signer, 400).unwrap();
        assert_eq!(signature, None);
        assert_eq!(cumulative, 75);
        assert_eq!(expires_at, 0);

        assert!(close_voucher(&state, 50, &signer, 500).is_err());

        state.highest_voucher_expires_at = Some(0);
        let (signature, cumulative, expires_at) =
            close_voucher(&state, 50, &signer, i64::MAX).unwrap();
        assert_eq!(signature, Some([7_u8; 64]));
        assert_eq!(cumulative, 75);
        assert_eq!(expires_at, 0);
    }
}
