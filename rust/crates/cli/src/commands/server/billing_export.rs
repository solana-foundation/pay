//! Non-blocking billing-event export.
//!
//! `record_exchange` hands a completed [`pay_core::BillingEvent`] to
//! [`BillingSink::report`], a cheap, bounded, drop-on-full channel send —
//! never anything that could stall the request/response cycle that produced
//! it. A background task drains that channel and `XADD`s each event into a
//! Redis Stream (`pay-worker`'s `report-billing-events` job reads from
//! there, batches, and ships them onward). Entirely opt-in: unset
//! `PAY_BILLING_REDIS_URL` and `BillingSink::from_env` returns `None`, at
//! which point `record_exchange` pays nothing beyond an `Option` check.

use std::time::Duration;

use pay_core::BillingEvent;
use tokio::sync::mpsc;

/// Redis Stream key billing events are `XADD`ed into. `pay-worker`'s
/// consumer job reads from the same key.
const STREAM_KEY: &str = "pay:billing:events";

/// Bounded channel capacity between `record_exchange` (producer, in the hot
/// path) and the background Redis writer (consumer). A full channel means
/// the writer is falling behind Redis or Redis is down — dropping the event
/// is the correct failure mode, not blocking the request that produced it.
const CHANNEL_CAPACITY: usize = 4096;

/// Defensive cap on the stream's length (`XADD ... MAXLEN ~`), trimmed from
/// the oldest end. `report-billing-events` also `XDEL`s each entry right
/// after it acks it, so in steady state the stream never approaches this —
/// it only bites during an extended consumer outage, trading the oldest
/// unreported events for a bounded memory footprint rather than letting the
/// stream grow without limit.
const MAX_STREAM_LEN: u64 = 200_000;

/// Backoff between Redis connect attempts, capped. Applies only to
/// establishing the connection — once connected, `ConnectionManager`
/// transparently retries transient per-command failures on its own, so
/// there is no separate "reconnect after a failed XADD" path here.
const CONNECT_RETRY_INITIAL: Duration = Duration::from_secs(1);
const CONNECT_RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct BillingSink {
    tx: mpsc::Sender<BillingEvent>,
}

impl BillingSink {
    /// Starts the background Redis writer if `PAY_BILLING_REDIS_URL` is set
    /// and non-empty. Returns `None` otherwise — the feature is entirely
    /// opt-in per deployment.
    pub fn from_env() -> Option<Self> {
        let redis_url = std::env::var("PAY_BILLING_REDIS_URL")
            .ok()
            .filter(|v| !v.is_empty())?;
        Some(Self::spawn(redis_url))
    }

    fn spawn(redis_url: String) -> Self {
        Self::spawn_with_stream_cap(redis_url, MAX_STREAM_LEN)
    }

    /// Split out from `spawn` so tests can exercise `MAXLEN` trimming with a
    /// small cap instead of needing to flood 200,000 real entries.
    fn spawn_with_stream_cap(redis_url: String, max_stream_len: u64) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        tokio::spawn(drain(redis_url, rx, max_stream_len));
        Self { tx }
    }

    /// Report one metered exchange. Non-blocking: drops the event and logs
    /// (never panics, never awaits) if the channel is full or the writer
    /// task has exited.
    pub fn report(&self, event: BillingEvent) {
        if self.tx.try_send(event).is_err() {
            tracing::warn!(
                metric = "pay_billing_events_dropped_total",
                "billing-event channel full or closed; dropping event"
            );
        }
    }
}

/// Owns the Redis connection and the receiving half of the channel for the
/// lifetime of the process. Runs until the last [`BillingSink`] clone (and
/// thus every sender) is dropped.
///
/// Connects with capped exponential backoff rather than giving up after one
/// attempt — a gateway that starts before Redis is reachable (or during a
/// Redis restart) must recover on its own once Redis comes back, not drop
/// every event for the rest of the process's life. The retry loop blocks
/// only this background task, never the producer: while it's retrying, the
/// bounded channel simply fills and `BillingSink::report`'s `try_send`
/// starts dropping events — the same backpressure behavior as a slow
/// consumer, never a block on the request path that produced them.
async fn drain(redis_url: String, mut rx: mpsc::Receiver<BillingEvent>, max_stream_len: u64) {
    let mut conn = connect_with_backoff(&redis_url).await;
    while let Some(event) = rx.recv().await {
        let payload = match serde_json::to_string(&event) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(%error, "billing-event export: failed to serialize event");
                continue;
            }
        };
        let result: Result<String, redis::RedisError> = redis::cmd("XADD")
            .arg(STREAM_KEY)
            .arg("MAXLEN")
            .arg("~")
            .arg(max_stream_len)
            .arg("*")
            .arg("event")
            .arg(payload)
            .query_async(&mut conn)
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, "billing-event export: XADD failed; dropping event");
        }
    }
}

/// Retries [`connect`] with capped exponential backoff until it succeeds.
/// Never gives up — the caller (`drain`) has no fallback path, so an
/// unreachable Redis must be retried indefinitely rather than abandoned.
async fn connect_with_backoff(redis_url: &str) -> redis::aio::ConnectionManager {
    let mut backoff = CONNECT_RETRY_INITIAL;
    loop {
        match connect(redis_url).await {
            Ok(conn) => return conn,
            Err(error) => {
                tracing::error!(
                    %error,
                    retry_in_secs = backoff.as_secs(),
                    "billing-event export: failed to connect to Redis, retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(CONNECT_RETRY_MAX);
            }
        }
    }
}

async fn connect(redis_url: &str) -> Result<redis::aio::ConnectionManager, redis::RedisError> {
    let client = redis::Client::open(redis_url)?;
    client.get_connection_manager().await
}

/// Real-Redis tests (no mocks) for the two failure modes a prior review
/// found: a `drain` that gives up forever after one failed connect, and a
/// stream that grows without bound. Each spawns an ephemeral `redis-server`
/// subprocess; tests skip (rather than fail) when it isn't on `PATH`.
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    fn redis_server_available() -> bool {
        Command::new("redis-server")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn ephemeral_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind an ephemeral port")
            .local_addr()
            .expect("local addr")
            .port()
    }

    /// RAII guard around the spawned `redis-server` process. `Child` alone
    /// leaves the process running (a zombie, once the test process exits)
    /// if a test panics before an explicit kill+wait — `Drop` guarantees
    /// cleanup on every exit path, panic included.
    struct RedisGuard(Child);

    impl Drop for RedisGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn start_redis_on(port: u16) -> RedisGuard {
        let child = Command::new("redis-server")
            .args([
                "--port",
                &port.to_string(),
                "--save",
                "",
                "--appendonly",
                "no",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn redis-server");
        // Wrap immediately, before the readiness-polling loop below — the
        // panic on timeout must also drop a guarded `RedisGuard` (whose
        // `Drop` kills+waits), never a bare `Child`.
        let guard = RedisGuard(child);
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).expect("valid url");
        for _ in 0..50 {
            if client.get_connection().is_ok() {
                return guard;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("redis-server on port {port} did not become ready in time");
    }

    fn sample_event() -> BillingEvent {
        BillingEvent {
            method: "POST".to_string(),
            path: "v1/simple/echo".to_string(),
            host: Some("vision.google.example.com".to_string()),
            subdomain: "vision".to_string(),
            status: 200,
            ms: 1,
            scheme: pay_types::metering::Scheme::MppCharge,
            charge_status: pay_core::ChargeStatus::Charged,
            currency: Some("SOL".to_string()),
            amount_usd: Some(0.01),
            unit: Some("requests".to_string()),
            quantity: None,
        }
    }

    async fn connect_multiplexed(port: u16) -> redis::aio::MultiplexedConnection {
        redis::Client::open(format!("redis://127.0.0.1:{port}"))
            .expect("valid url")
            .get_multiplexed_async_connection()
            .await
            .expect("connect")
    }

    async fn stream_len(conn: &mut redis::aio::MultiplexedConnection) -> u64 {
        redis::cmd("XLEN")
            .arg(STREAM_KEY)
            .query_async(conn)
            .await
            .unwrap_or(0)
    }

    async fn wait_until_len_at_least(
        conn: &mut redis::aio::MultiplexedConnection,
        expected: u64,
        timeout: Duration,
    ) -> u64 {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let len = stream_len(conn).await;
            if len >= expected || tokio::time::Instant::now() >= deadline {
                return len;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn a_reported_event_is_xadded_once_connected() {
        if !redis_server_available() {
            eprintln!("skipping: redis-server not found on PATH");
            return;
        }
        let port = ephemeral_port();
        let _server = start_redis_on(port);

        let sink = BillingSink::spawn_with_stream_cap(format!("redis://127.0.0.1:{port}"), 1000);
        sink.report(sample_event());

        let mut conn = connect_multiplexed(port).await;
        let len = wait_until_len_at_least(&mut conn, 1, Duration::from_secs(5)).await;
        assert_eq!(len, 1);
    }

    /// Regression test for "startup failure disables export": `drain` used
    /// to make exactly one connect attempt and discard every event for the
    /// rest of the process's life if it failed. This points a sink at a
    /// port with NOTHING listening, lets the first (failing) connect
    /// attempt run, only THEN starts Redis on that exact port, and asserts
    /// the event queued during the outage is still delivered.
    #[tokio::test]
    async fn drain_reconnects_once_redis_becomes_reachable() {
        if !redis_server_available() {
            eprintln!("skipping: redis-server not found on PATH");
            return;
        }
        let port = ephemeral_port();

        let sink = BillingSink::spawn_with_stream_cap(format!("redis://127.0.0.1:{port}"), 1000);
        sink.report(sample_event());

        // Let the first, necessarily-failing connect attempt actually run
        // before Redis exists on this port at all.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let _server = start_redis_on(port);
        let mut conn = connect_multiplexed(port).await;
        let len = wait_until_len_at_least(&mut conn, 1, Duration::from_secs(10)).await;
        assert_eq!(
            len, 1,
            "an event queued before Redis was reachable must still be delivered once it recovers"
        );
    }

    /// Regression test for "Redis stream grows forever": asserts `XADD
    /// ... MAXLEN ~` actually bounds the stream rather than letting every
    /// entry accumulate.
    #[tokio::test]
    async fn xadd_trims_the_stream_toward_the_configured_cap() {
        if !redis_server_available() {
            eprintln!("skipping: redis-server not found on PATH");
            return;
        }
        let port = ephemeral_port();
        let _server = start_redis_on(port);

        const CAP: u64 = 5;
        const TOTAL: usize = 40;
        let sink = BillingSink::spawn_with_stream_cap(format!("redis://127.0.0.1:{port}"), CAP);
        for _ in 0..TOTAL {
            sink.report(sample_event());
        }

        let mut conn = connect_multiplexed(port).await;
        // Wait for at least CAP entries to land, then let a couple more
        // XADDs (each of which can trigger trimming) settle.
        wait_until_len_at_least(&mut conn, CAP, Duration::from_secs(5)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let len = stream_len(&mut conn).await;

        // `MAXLEN ~` trims in whole internal nodes for performance, so the
        // result is approximate, not exact — assert it stayed bounded well
        // below the full unbounded count, not pinned to CAP precisely.
        assert!(
            len < TOTAL as u64,
            "expected trimming to bound the stream below all {TOTAL} XADDed entries, got {len}"
        );
    }
}
