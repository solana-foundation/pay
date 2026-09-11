//! `report-billing-events` — drains the `pay:billing:events` Redis Stream
//! (populated by `pay-proxy`'s hosts via `record_exchange`, one entry per
//! metered exchange regardless of payment scheme) and durably relocates
//! each one into its own `pay:billing:events:exported:<id>` key.
//!
//! The final export destination is intentionally not wired up yet: whether a
//! self-hosted, analytics-only Apigee Adapter feed is acceptable for a
//! partner's billing meter (vs. requiring traffic through their fully-hosted
//! proxy) is still an open question with that partner. Until that's
//! answered, `report` doesn't make an outbound call at all — it relocates
//! the event, byte-for-byte, into a durable, verifiable per-event key: the
//! record survives under a different key, rather than being deleted from
//! the inbox on nothing more than a log line. An entry is only acked (and
//! removed from the inbox) once that relocation actually succeeds.
//!
//! Deliberately one `SET` per event, keyed by the source stream id, rather
//! than a second stream: a retry after a crash between the relocation
//! write and the source ack/delete (or after a `report` failure that
//! itself needs retrying) writes the exact same key with the exact same
//! value again, which is a safe no-op. Appending to a stream instead — even
//! reusing the source id as the new entry's id — cannot give that guarantee
//! *in general*, because Redis Streams enforce one global, monotonically
//! increasing id per stream: by the time a stuck entry is retried, other
//! (newer) events may already have advanced the archive stream's id past
//! the stuck entry's own id, so a same-id retry would fail for a reason
//! indistinguishable from "already delivered" — and treating that as
//! success would silently drop a record that was never actually
//! relocated. `report` is the one function that needs to change — to a
//! real outbound call, reading from the inbox or from these keys — once
//! the partner's requirements are resolved.
//!
//! Uses a single, stable consumer identity for the life of the process
//! (`HOSTNAME`, falling back to the PID) rather than a fresh one per poll —
//! Redis never forgets a consumer group's member list on its own, so a
//! per-call identity leaks unbounded `XINFO CONSUMERS` metadata. Every
//! drain reclaims stale-pending entries (delivered to a since-crashed
//! consumer, never acked) via `XAUTOCLAIM` before reading new ones, and a
//! single "drain" (one `RUN_ONCE` invocation, or one continuous-mode tick)
//! loops until the stream is fully caught up rather than stopping after one
//! batch.
//!
//! Env:
//!   PAY_BILLING_REDIS_URL             Redis connection URL (required)
//!   RUN_ONCE                          default true; set false for the
//!                                     continuous Cloud Run service form
//!   BILLING_EXPORT_INTERVAL_SECONDS   poll interval in continuous mode
//!                                     (default 10)
//!   BILLING_EXPORT_BATCH_SIZE         max entries per XREADGROUP/XAUTOCLAIM
//!                                     call (default 200)
//!   BILLING_EXPORT_MIN_IDLE_MS        how long an entry must sit
//!                                     unacknowledged under another
//!                                     consumer before this one reclaims it
//!                                     (default 60000)
//!   PORT                              health-check port in continuous mode
//!                                     (default 8080)

use std::collections::HashMap;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use pay_worker::error::JobError;
use pay_worker::telemetry;
use redis::Value;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Redis Stream key `pay-proxy` hosts `XADD` billing events into (mirrors
/// the producer-side constant in `pay::commands::server::billing_export`).
const STREAM_KEY: &str = "pay:billing:events";
/// Prefix for the durable per-event keys `report` relocates
/// successfully-handled events into (`{PREFIX}{source stream id}`).
/// Deliberately given no TTL/expiry — each key IS the delivery guarantee
/// for one billing event; expiring them silently would recreate the exact
/// "records vanish with nothing to show for it" problem this design
/// exists to avoid. Retention/cleanup is a separate, explicit operational
/// decision to make once a real downstream exporter exists and is
/// actually consuming them.
const EXPORTED_KEY_PREFIX: &str = "pay:billing:events:exported:";
const CONSUMER_GROUP: &str = "report-billing-events";
const DEFAULT_INTERVAL_SECONDS: u64 = 10;
const DEFAULT_BATCH_SIZE: u64 = 200;
const DEFAULT_MIN_IDLE_MS: u64 = 60_000;
const DEFAULT_PORT: u64 = 8080;

/// Mirrors `pay_core::BillingEvent`'s JSON shape. Deliberately not a shared
/// type: this job is the one place allowed to know a downstream billing
/// reporting pipeline exists at all, and pulling in pay-core's full
/// dependency tree just to reuse one struct would be a heavier coupling
/// than a documented wire-format convention.
#[derive(Debug, serde::Deserialize)]
struct BillingEvent {
    method: String,
    path: String,
    // `#[serde(default)]` on the two fields below: entries already queued
    // from before this field existed on the producer side must still
    // decode, not be treated as poison messages and dropped.
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    subdomain: Option<String>,
    status: u16,
    #[allow(dead_code)] // carried through for a future export payload
    ms: u64,
    scheme: String,
    charge_status: String,
    currency: Option<String>,
    amount_usd: Option<f64>,
    unit: Option<String>,
    quantity: Option<u64>,
}

/// `report`'s failure mode: relocating the event into the exported archive
/// failed. Real, not a formality — `report` makes an actual Redis call
/// that can actually fail, and the ack-only-on-success wiring below
/// depends on that.
#[derive(Debug, thiserror::Error)]
#[error("billing event report failed: {0}")]
struct ReportError(String);

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let _telemetry = telemetry::init("pay-jobs-report-billing-events");

    let redis_url = match std::env::var("PAY_BILLING_REDIS_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => {
            return record_startup_failure(JobError::Config(
                "PAY_BILLING_REDIS_URL must be set".to_string(),
            ));
        }
    };
    let run_once = match parse_bool_env("RUN_ONCE", true) {
        Ok(value) => value,
        Err(error) => return record_startup_failure(error),
    };
    let batch_size = match parse_u64_env("BILLING_EXPORT_BATCH_SIZE", DEFAULT_BATCH_SIZE) {
        Ok(value) => value,
        Err(error) => return record_startup_failure(error),
    };
    let min_idle_ms = match parse_u64_env("BILLING_EXPORT_MIN_IDLE_MS", DEFAULT_MIN_IDLE_MS) {
        Ok(value) => value,
        Err(error) => return record_startup_failure(error),
    };
    let consumer = consumer_identity();

    let mut conn = match connect(&redis_url).await {
        Ok(conn) => conn,
        Err(error) => return record_startup_failure(error),
    };
    if let Err(error) = ensure_group(&mut conn).await {
        return record_startup_failure(error);
    }

    if run_once {
        return match drain_all(&mut conn, &consumer, batch_size, min_idle_ms).await {
            Ok(count) => {
                info!(
                    count,
                    event = "report_billing_events_exit",
                    outcome = "ok",
                    "drained available billing events"
                );
                std::process::ExitCode::SUCCESS
            }
            Err(error) => record_startup_failure(error),
        };
    }

    let interval_seconds =
        match parse_u64_env("BILLING_EXPORT_INTERVAL_SECONDS", DEFAULT_INTERVAL_SECONDS) {
            Ok(0) => {
                return record_startup_failure(JobError::Config(
                    "BILLING_EXPORT_INTERVAL_SECONDS must be greater than zero".into(),
                ));
            }
            Ok(value) => value,
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
        consumer = %consumer,
        "continuous report-billing-events worker starting"
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
        info!("report-billing-events shutdown signal received");
        cancel_on_signal.cancel();
    });

    let mut ticker = tokio::time::interval(Duration::from_secs(interval_seconds));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }
        match drain_all(&mut conn, &consumer, batch_size, min_idle_ms).await {
            Ok(count) if count > 0 => info!(count, "reported billing events"),
            Ok(_) => {}
            Err(error) => warn!(%error, "billing-event drain failed; will retry next tick"),
        }
        if cancel.is_cancelled() {
            break;
        }
    }
    cancel.cancel();

    let server_result = server.await;
    if !signal.is_finished() {
        signal.abort();
    }
    let _ = signal.await;

    match server_result {
        Ok(Ok(())) => std::process::ExitCode::SUCCESS,
        Ok(Err(error)) => {
            error!(%error, "report-billing-events health server failed");
            std::process::ExitCode::FAILURE
        }
        Err(error) => {
            error!(%error, "report-billing-events health server task failed");
            std::process::ExitCode::FAILURE
        }
    }
}

/// A single, stable identity reused for the life of the process. Cloud
/// Run sets `HOSTNAME` to a value stable for the life of one instance —
/// exactly the granularity a Redis consumer group member should have.
/// Falls back to the PID (also process-lifetime-stable) when unset, e.g.
/// running locally.
fn consumer_identity() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::process::id().to_string())
}

async fn connect(redis_url: &str) -> Result<redis::aio::ConnectionManager, JobError> {
    let client = redis::Client::open(redis_url)
        .map_err(|error| JobError::Config(format!("Redis client: {error}")))?;
    client
        .get_connection_manager()
        .await
        .map_err(|error| JobError::Config(format!("Redis connect: {error}")))
}

/// Idempotent: `BUSYGROUP` (the group already exists from a prior run) is
/// not an error.
async fn ensure_group(conn: &mut redis::aio::ConnectionManager) -> Result<(), JobError> {
    let result: Result<String, redis::RedisError> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .arg("0")
        .arg("MKSTREAM")
        .query_async(conn)
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("BUSYGROUP") => Ok(()),
        Err(error) => Err(JobError::Config(format!("Redis XGROUP CREATE: {error}"))),
    }
}

/// Drains repeatedly until a pass reclaims and reads nothing at all. One
/// `RUN_ONCE` invocation, or one continuous-mode tick, must not report
/// success while entries beyond a single batch remain available — each
/// underlying `drain_once` call is bounded by `batch_size`, so catching up
/// after any real backlog takes more than one call.
async fn drain_all(
    conn: &mut redis::aio::ConnectionManager,
    consumer: &str,
    batch_size: u64,
    min_idle_ms: u64,
) -> Result<usize, JobError> {
    let mut total = 0usize;
    loop {
        let count = drain_once(conn, consumer, batch_size, min_idle_ms).await?;
        if count == 0 {
            return Ok(total);
        }
        total += count;
    }
}

/// Reclaims stale-pending entries (delivered to a consumer that crashed or
/// was replaced before acking) before reading newly-arrived ones. Returns
/// the total number of entries acknowledged across both.
async fn drain_once(
    conn: &mut redis::aio::ConnectionManager,
    consumer: &str,
    batch_size: u64,
    min_idle_ms: u64,
) -> Result<usize, JobError> {
    let reclaimed = reclaim_pending(conn, consumer, batch_size, min_idle_ms).await?;
    let read = read_new(conn, consumer, batch_size).await?;
    Ok(reclaimed + read)
}

/// `XAUTOCLAIM`s up to `batch_size` entries that have been pending (
/// delivered, never acked) for at least `min_idle_ms` under any consumer,
/// reassigns them to `consumer`, and processes them exactly like freshly
/// read ones. Always scans from the start of the pending list (`0-0`): a
/// reclaimed-and-then-acked entry leaves the pending list entirely, so
/// repeated calls converge without needing to track `XAUTOCLAIM`'s cursor.
async fn reclaim_pending(
    conn: &mut redis::aio::ConnectionManager,
    consumer: &str,
    batch_size: u64,
    min_idle_ms: u64,
) -> Result<usize, JobError> {
    let reply: Value = redis::cmd("XAUTOCLAIM")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .arg(consumer)
        .arg(min_idle_ms)
        .arg("0-0")
        .arg("COUNT")
        .arg(batch_size)
        .query_async(conn)
        .await
        .map_err(|error| JobError::Config(format!("Redis XAUTOCLAIM: {error}")))?;
    // Reply shape: [next-cursor, [[id, [field, value, ...]], ...], deleted-ids?].
    let Value::Array(parts) = reply else {
        return Ok(0);
    };
    let entries = match parts.get(1) {
        Some(entries_value) => parse_claimed_entries(entries_value),
        None => Vec::new(),
    };
    process_entries(conn, &entries).await
}

/// Reads newly-arrived entries (`>`) via the consumer group and processes
/// them.
async fn read_new(
    conn: &mut redis::aio::ConnectionManager,
    consumer: &str,
    batch_size: u64,
) -> Result<usize, JobError> {
    let reply: redis::streams::StreamReadReply = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(CONSUMER_GROUP)
        .arg(consumer)
        .arg("COUNT")
        .arg(batch_size)
        .arg("STREAMS")
        .arg(STREAM_KEY)
        .arg(">")
        .query_async(conn)
        .await
        .map_err(|error| JobError::Config(format!("Redis XREADGROUP: {error}")))?;

    let mut entries = Vec::new();
    for stream_key in &reply.keys {
        for entry in &stream_key.ids {
            entries.push((entry.id.clone(), entry.map.clone()));
        }
    }
    process_entries(conn, &entries).await
}

/// Reports each entry and only then `XACK`s + `XDEL`s it. A malformed
/// entry (undecodable, or missing its `event` field) is acked and deleted
/// anyway — logged, not retried — since no amount of retrying will make a
/// permanently poison message decodable; a `report` failure, in contrast,
/// leaves the entry pending so the next drain retries it (or, eventually,
/// `reclaim_pending` does, once it has aged past `min_idle_ms`).
async fn process_entries(
    conn: &mut redis::aio::ConnectionManager,
    entries: &[(String, HashMap<String, Value>)],
) -> Result<usize, JobError> {
    if entries.is_empty() {
        return Ok(0);
    }
    let mut ids_to_ack = Vec::new();
    for (id, fields) in entries {
        let handled = match decode_entry(fields) {
            Some((raw, Ok(event))) => match report(conn, id, &raw, &event).await {
                Ok(()) => true,
                Err(error) => {
                    warn!(%error, %id, "billing event report failed; leaving pending for retry");
                    false
                }
            },
            Some((_, Err(error))) => {
                warn!(%error, %id, "failed to decode billing event; dropping (poison message)");
                true
            }
            None => {
                warn!(%id, "billing-event entry missing its 'event' field; dropping");
                true
            }
        };
        if handled {
            ids_to_ack.push(id.clone());
        }
    }
    if ids_to_ack.is_empty() {
        return Ok(0);
    }

    let mut ack = redis::cmd("XACK");
    ack.arg(STREAM_KEY).arg(CONSUMER_GROUP);
    for id in &ids_to_ack {
        ack.arg(id);
    }
    let _: i64 = ack
        .query_async(conn)
        .await
        .map_err(|error| JobError::Config(format!("Redis XACK: {error}")))?;

    // XACK only clears the consumer group's pending list — it does not
    // shrink the stream itself. Without this, every successfully handled
    // entry (on top of whatever the producer's XADD ... MAXLEN trims)
    // stays in Redis forever.
    let mut del = redis::cmd("XDEL");
    del.arg(STREAM_KEY);
    for id in &ids_to_ack {
        del.arg(id);
    }
    let _: i64 = del
        .query_async(conn)
        .await
        .map_err(|error| JobError::Config(format!("Redis XDEL: {error}")))?;

    Ok(ids_to_ack.len())
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}

/// Parses a raw `[field, value, field, value, ...]` reply array (the shape
/// `XAUTOCLAIM`/`XRANGE`/`XCLAIM` return per entry) into a field map — the
/// same shape `redis::streams::StreamId::map` already gives `XREADGROUP`
/// callers, so `decode_entry` can treat both sources identically.
fn parse_fields_array(value: &Value) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    if let Value::Array(items) = value {
        let mut iter = items.iter();
        while let (Some(key_value), Some(value)) = (iter.next(), iter.next()) {
            if let Some(key) = value_as_string(key_value) {
                map.insert(key, value.clone());
            }
        }
    }
    map
}

/// Parses `XAUTOCLAIM`'s entries array (`reply[1]`) into `(id, fields)`
/// pairs.
fn parse_claimed_entries(value: &Value) -> Vec<(String, HashMap<String, Value>)> {
    let Value::Array(entries) = value else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let Value::Array(parts) = entry else {
                return None;
            };
            let id = value_as_string(parts.first()?)?;
            let fields = parts.get(1).map(parse_fields_array).unwrap_or_default();
            Some((id, fields))
        })
        .collect()
}

/// `None` if the entry carries no `event` field at all. Otherwise the
/// original raw JSON string alongside the decode result — `report` needs
/// the raw string to relocate the event byte-for-byte into its exported
/// key, without a lossy re-encode through `BillingEvent`.
fn decode_entry(
    fields: &HashMap<String, Value>,
) -> Option<(String, Result<BillingEvent, serde_json::Error>)> {
    let raw = value_as_string(fields.get("event")?)?;
    let decoded = serde_json::from_str(&raw);
    Some((raw, decoded))
}

/// TODO(apigee): this is the one place that needs to change once the
/// partner confirms whether the self-hosted, analytics-only Adapter feed is
/// acceptable for their billing meter — to a real outbound call, either
/// instead of or in addition to the relocation below.
///
/// Until then, the relocation itself is the real delivery guarantee: the
/// event is `SET` into `{EXPORTED_KEY_PREFIX}{id}` — a durable, verifiable
/// key a real exporter can later read — before the caller acks and removes
/// it from the inbox. Keying by the source entry's own id (rather than
/// appending to a second stream) is what makes this idempotent: a retry
/// after a crash between this write and the source ack/delete (or after an
/// earlier failed attempt) writes the exact same key with the exact same
/// value again — a safe no-op, never a duplicate. `raw_json` is forwarded
/// byte-for-byte (not a re-encode of `event`) so the exported record is
/// exactly what the producer sent.
async fn report(
    conn: &mut redis::aio::ConnectionManager,
    id: &str,
    raw_json: &str,
    event: &BillingEvent,
) -> Result<(), ReportError> {
    info!(
        monotonic_counter.pay_billing_events_reported_total = 1_u64,
        method = %event.method,
        path = %event.path,
        host = event.host.as_deref(),
        subdomain = event.subdomain.as_deref(),
        status = event.status,
        scheme = %event.scheme,
        charge_status = %event.charge_status,
        currency = event.currency.as_deref(),
        amount_usd = event.amount_usd,
        unit = event.unit.as_deref(),
        quantity = event.quantity,
        "billing event"
    );
    let result: Result<(), redis::RedisError> = redis::cmd("SET")
        .arg(format!("{EXPORTED_KEY_PREFIX}{id}"))
        .arg(raw_json)
        .query_async(conn)
        .await;
    result.map_err(|error| ReportError(error.to_string()))
}

fn record_startup_failure(error: JobError) -> std::process::ExitCode {
    error!(
        monotonic_counter.pay_billing_export_startup_failures_total = 1_u64,
        event = "report_billing_events_exit",
        outcome = "aborted",
        %error,
        "report-billing-events failed to start"
    );
    std::process::ExitCode::FAILURE
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
