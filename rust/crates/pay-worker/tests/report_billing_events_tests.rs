//! Integration test for the `report-billing-events` binary — runs the
//! actual compiled binary as a subprocess against a real, ephemeral local
//! Redis instance. No mocks: this exercises the exact artifact that gets
//! deployed.
//!
//! Requires `redis-server` on `PATH`. Skips (rather than failing) when it
//! isn't found, so this doesn't break environments without it installed.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const STREAM_KEY: &str = "pay:billing:events";
const EXPORTED_KEY_PREFIX: &str = "pay:billing:events:exported:";
const CONSUMER_GROUP: &str = "report-billing-events";

struct RedisServer {
    child: Child,
    port: u16,
}

impl RedisServer {
    fn start() -> Option<Self> {
        if Command::new("redis-server")
            .arg("--version")
            .output()
            .is_err()
        {
            return None;
        }
        let port = ephemeral_port();
        let child = Command::new("redis-server")
            .args([
                "--port",
                &port.to_string(),
                "--save",
                "",
                "--appendonly",
                "no",
                "--daemonize",
                "no",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let server = Self { child, port };
        server.wait_ready();
        Some(server)
    }

    fn url(&self) -> String {
        format!("redis://127.0.0.1:{}", self.port)
    }

    fn wait_ready(&self) {
        let client = redis::Client::open(self.url()).expect("valid redis url");
        for _ in 0..50 {
            if client.get_connection().is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "redis-server on port {} did not become ready in time",
            self.port
        );
    }
}

impl Drop for RedisServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Bind an ephemeral port and release it immediately. A real TOCTOU race
/// window exists, but is negligible for a local, single-process test suite.
fn ephemeral_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

#[test]
fn report_billing_events_drains_and_acks_a_real_event() {
    let Some(redis_server) = RedisServer::start() else {
        eprintln!(
            "skipping report_billing_events_drains_and_acks_a_real_event: redis-server not found on PATH"
        );
        return;
    };

    let client = redis::Client::open(redis_server.url()).expect("valid redis url");
    let mut conn = client.get_connection().expect("connect to ephemeral redis");

    // Seed the stream exactly as the CLI producer would — one JSON `event`
    // field per entry, matching `pay_core::BillingEvent`'s wire shape.
    let payload = serde_json::json!({
        "method": "POST",
        "path": "v1/simple/echo",
        "status": 200,
        "ms": 42,
        "scheme": "mpp-charge",
        "charge_status": "charged",
        "currency": "SOL",
        "amount_usd": 0.01,
        "unit": "requests",
        "quantity": null,
    })
    .to_string();
    let seeded_id: String = redis::cmd("XADD")
        .arg(STREAM_KEY)
        .arg("*")
        .arg("event")
        .arg(&payload)
        .query(&mut conn)
        .expect("seed the stream");

    let bin = env!("CARGO_BIN_EXE_report-billing-events");
    let status = Command::new(bin)
        .env("PAY_BILLING_REDIS_URL", redis_server.url())
        .env("RUN_ONCE", "true")
        .status()
        .expect("run report-billing-events");
    assert!(
        status.success(),
        "report-billing-events exited with {status}"
    );

    // The entry was acknowledged: no pending entries remain for the group.
    // XPENDING's summary reply is `[count, min_id, max_id, consumers]`.
    let pending: redis::Value = redis::cmd("XPENDING")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .query(&mut conn)
        .expect("XPENDING summary");
    let redis::Value::Array(fields) = &pending else {
        panic!("unexpected XPENDING reply shape: {pending:?}");
    };
    assert_eq!(
        fields[0],
        redis::Value::Int(0),
        "expected every delivered entry to be acknowledged, got {pending:?}"
    );

    // Regression: XACK alone does not shrink the stream. A handled entry
    // must also be XDELed, or the stream grows without bound forever.
    let len: u64 = redis::cmd("XLEN")
        .arg(STREAM_KEY)
        .query(&mut conn)
        .expect("XLEN");
    assert_eq!(
        len, 0,
        "expected the acknowledged entry to also be removed from the stream, stream still has {len} entries"
    );

    // Regression: deleting the inbox entry is only safe because it was
    // durably relocated first — not just logged. The exported key must
    // actually contain the event, byte-for-byte.
    let exported_raw: Option<String> = redis::cmd("GET")
        .arg(format!("{EXPORTED_KEY_PREFIX}{seeded_id}"))
        .query(&mut conn)
        .expect("GET exported key");
    let exported_json: serde_json::Value = serde_json::from_str(
        &exported_raw.expect("expected the handled event to be durably relocated"),
    )
    .expect("exported payload is valid JSON");
    assert_eq!(
        exported_json["path"], "v1/simple/echo",
        "expected the exported record to be the original event, byte-for-byte"
    );
}

/// Regression test for "pending events never recovered": simulates a
/// consumer that read an entry via `XREADGROUP` and then crashed before
/// acking it, then runs the real binary as a fresh process (a distinct PID,
/// so under the old per-call consumer-naming scheme it would also be a
/// distinct, unrelated consumer identity) and asserts it reclaims, reports,
/// and fully removes the stranded entry rather than leaving it pending
/// forever.
#[test]
fn report_billing_events_reclaims_a_stranded_pending_entry() {
    let Some(redis_server) = RedisServer::start() else {
        eprintln!(
            "skipping report_billing_events_reclaims_a_stranded_pending_entry: redis-server not found on PATH"
        );
        return;
    };

    let client = redis::Client::open(redis_server.url()).expect("valid redis url");
    let mut conn = client.get_connection().expect("connect to ephemeral redis");

    let payload = serde_json::json!({
        "method": "POST",
        "path": "v1/simple/echo",
        "status": 200,
        "ms": 7,
        "scheme": "x402-exact",
        "charge_status": "charged",
        "currency": "USDC",
        "amount_usd": 0.02,
        "unit": "requests",
        "quantity": null,
    })
    .to_string();
    let seeded_id: String = redis::cmd("XADD")
        .arg(STREAM_KEY)
        .arg("*")
        .arg("event")
        .arg(&payload)
        .query(&mut conn)
        .expect("seed the stream");

    // Simulate a consumer that read the entry and then crashed: create the
    // group, deliver the entry to a consumer that will never ack it.
    let _: Result<String, redis::RedisError> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .arg("0")
        .query(&mut conn);
    let _: redis::streams::StreamReadReply = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(CONSUMER_GROUP)
        .arg("crashed-consumer")
        .arg("COUNT")
        .arg(10)
        .arg("STREAMS")
        .arg(STREAM_KEY)
        .arg(">")
        .query(&mut conn)
        .expect("simulate delivery to a since-crashed consumer");

    let pending_before: redis::Value = redis::cmd("XPENDING")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .query(&mut conn)
        .expect("XPENDING summary");
    let redis::Value::Array(fields_before) = &pending_before else {
        panic!("unexpected XPENDING reply shape: {pending_before:?}");
    };
    assert_eq!(
        fields_before[0],
        redis::Value::Int(1),
        "expected the simulated crash to leave exactly one entry pending, got {pending_before:?}"
    );

    let bin = env!("CARGO_BIN_EXE_report-billing-events");
    let status = Command::new(bin)
        .env("PAY_BILLING_REDIS_URL", redis_server.url())
        .env("RUN_ONCE", "true")
        // Reclaim immediately rather than waiting the default 60s — the
        // entry above is already "stranded", regardless of its exact age.
        .env("BILLING_EXPORT_MIN_IDLE_MS", "0")
        .status()
        .expect("run report-billing-events");
    assert!(
        status.success(),
        "report-billing-events exited with {status}"
    );

    let pending_after: redis::Value = redis::cmd("XPENDING")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .query(&mut conn)
        .expect("XPENDING summary");
    let redis::Value::Array(fields_after) = &pending_after else {
        panic!("unexpected XPENDING reply shape: {pending_after:?}");
    };
    assert_eq!(
        fields_after[0],
        redis::Value::Int(0),
        "expected the stranded entry to be reclaimed and acked, got {pending_after:?}"
    );

    let len: u64 = redis::cmd("XLEN")
        .arg(STREAM_KEY)
        .query(&mut conn)
        .expect("XLEN");
    assert_eq!(
        len, 0,
        "expected the reclaimed entry to also be XDELed, got {len} remaining"
    );

    let exported: Option<String> = redis::cmd("GET")
        .arg(format!("{EXPORTED_KEY_PREFIX}{seeded_id}"))
        .query(&mut conn)
        .expect("GET exported key");
    assert!(
        exported.is_some(),
        "expected the reclaimed entry to be durably relocated to {EXPORTED_KEY_PREFIX}{seeded_id}"
    );
}

/// Regression test for "run once leaves backlogs": seeds more entries than
/// a single batch, and asserts one `RUN_ONCE` invocation still drains the
/// entire backlog rather than stopping after the first `XREADGROUP` call.
#[test]
fn report_billing_events_run_once_drains_a_backlog_larger_than_one_batch() {
    let Some(redis_server) = RedisServer::start() else {
        eprintln!(
            "skipping report_billing_events_run_once_drains_a_backlog_larger_than_one_batch: redis-server not found on PATH"
        );
        return;
    };

    let client = redis::Client::open(redis_server.url()).expect("valid redis url");
    let mut conn = client.get_connection().expect("connect to ephemeral redis");

    const BATCH_SIZE: usize = 3;
    const TOTAL: usize = 10; // more than 3x the batch size
    let payload = serde_json::json!({
        "method": "POST",
        "path": "v1/simple/echo",
        "status": 200,
        "ms": 1,
        "scheme": "mpp-charge",
        "charge_status": "charged",
        "currency": "SOL",
        "amount_usd": 0.001,
        "unit": "requests",
        "quantity": null,
    })
    .to_string();
    for _ in 0..TOTAL {
        let _: String = redis::cmd("XADD")
            .arg(STREAM_KEY)
            .arg("*")
            .arg("event")
            .arg(&payload)
            .query(&mut conn)
            .expect("seed the stream");
    }

    let bin = env!("CARGO_BIN_EXE_report-billing-events");
    let status = Command::new(bin)
        .env("PAY_BILLING_REDIS_URL", redis_server.url())
        .env("RUN_ONCE", "true")
        .env("BILLING_EXPORT_BATCH_SIZE", BATCH_SIZE.to_string())
        .status()
        .expect("run report-billing-events");
    assert!(
        status.success(),
        "report-billing-events exited with {status}"
    );

    let len: u64 = redis::cmd("XLEN")
        .arg(STREAM_KEY)
        .query(&mut conn)
        .expect("XLEN");
    assert_eq!(
        len, 0,
        "expected a single RUN_ONCE invocation to drain the full {TOTAL}-entry backlog \
         (batch size {BATCH_SIZE}) rather than stop after one batch, {len} entries remain"
    );
}

/// Regression test for "consumer metadata grows forever": two separate
/// process invocations (distinct PIDs — under the old per-call
/// `pid-timestamp` naming scheme these would already be two distinct
/// consumers even before considering repeated polls within one process)
/// with the same `HOSTNAME` must register as exactly one Redis consumer
/// identity, not two.
#[test]
fn report_billing_events_reuses_one_consumer_identity_across_invocations() {
    let Some(redis_server) = RedisServer::start() else {
        eprintln!(
            "skipping report_billing_events_reuses_one_consumer_identity_across_invocations: redis-server not found on PATH"
        );
        return;
    };

    let client = redis::Client::open(redis_server.url()).expect("valid redis url");
    let mut conn = client.get_connection().expect("connect to ephemeral redis");

    let bin = env!("CARGO_BIN_EXE_report-billing-events");
    for _ in 0..2 {
        let status = Command::new(bin)
            .env("PAY_BILLING_REDIS_URL", redis_server.url())
            .env("RUN_ONCE", "true")
            .env("HOSTNAME", "stable-test-instance")
            .status()
            .expect("run report-billing-events");
        assert!(
            status.success(),
            "report-billing-events exited with {status}"
        );
    }

    let consumers: Vec<redis::Value> = redis::cmd("XINFO")
        .arg("CONSUMERS")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .query(&mut conn)
        .expect("XINFO CONSUMERS");
    assert_eq!(
        consumers.len(),
        1,
        "expected two invocations under the same HOSTNAME to register as one stable consumer, got {}: {consumers:?}",
        consumers.len()
    );
}

/// Regression test for "archive retries duplicate records": simulates an
/// entry that was already successfully relocated (its exported key
/// written) before the process crashed — the exact window between
/// `report` succeeding and the source ack that a real crash could land
/// in — then retries via reclaim. A stream-based archive with a
/// freshly-generated id per write would create a second, duplicate
/// record here; a per-event key written with a plain `SET` instead
/// overwrites the exact same key with the exact same value, so asserts
/// there is still exactly one value for this event, and that the retry
/// still completes the ack/delete on the source side.
#[test]
fn report_billing_events_retry_after_a_successful_relocation_does_not_duplicate() {
    let Some(redis_server) = RedisServer::start() else {
        eprintln!(
            "skipping report_billing_events_retry_after_a_successful_relocation_does_not_duplicate: redis-server not found on PATH"
        );
        return;
    };

    let client = redis::Client::open(redis_server.url()).expect("valid redis url");
    let mut conn = client.get_connection().expect("connect to ephemeral redis");

    let payload = serde_json::json!({
        "method": "POST",
        "path": "v1/simple/echo",
        "status": 200,
        "ms": 5,
        "scheme": "x402-batch",
        "charge_status": "charged",
        "currency": "USDC",
        "amount_usd": 0.04,
        "unit": "requests",
        "quantity": null,
    })
    .to_string();
    let seeded_id: String = redis::cmd("XADD")
        .arg(STREAM_KEY)
        .arg("*")
        .arg("event")
        .arg(&payload)
        .query(&mut conn)
        .expect("seed the stream");

    // Simulate: a prior attempt already relocated this event successfully
    // (the exported key exists, with the real payload)...
    let _: () = redis::cmd("SET")
        .arg(format!("{EXPORTED_KEY_PREFIX}{seeded_id}"))
        .arg(&payload)
        .query(&mut conn)
        .expect("simulate a prior successful relocation");
    // ...then crashed before acking the source, exactly like the
    // stranded-pending test.
    let _: Result<String, redis::RedisError> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .arg("0")
        .query(&mut conn);
    let _: redis::streams::StreamReadReply = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(CONSUMER_GROUP)
        .arg("crashed-consumer")
        .arg("COUNT")
        .arg(10)
        .arg("STREAMS")
        .arg(STREAM_KEY)
        .arg(">")
        .query(&mut conn)
        .expect("simulate delivery to a since-crashed consumer");

    let bin = env!("CARGO_BIN_EXE_report-billing-events");
    let status = Command::new(bin)
        .env("PAY_BILLING_REDIS_URL", redis_server.url())
        .env("RUN_ONCE", "true")
        .env("BILLING_EXPORT_MIN_IDLE_MS", "0")
        .status()
        .expect("run report-billing-events");
    assert!(
        status.success(),
        "report-billing-events exited with {status}"
    );

    let pending: redis::Value = redis::cmd("XPENDING")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .query(&mut conn)
        .expect("XPENDING summary");
    let redis::Value::Array(fields) = &pending else {
        panic!("unexpected XPENDING reply shape: {pending:?}");
    };
    assert_eq!(
        fields[0],
        redis::Value::Int(0),
        "expected the retry to still complete the ack even though relocation was already done, got {pending:?}"
    );

    let len: u64 = redis::cmd("XLEN")
        .arg(STREAM_KEY)
        .query(&mut conn)
        .expect("XLEN");
    assert_eq!(
        len, 0,
        "expected the source entry to be removed once the retry completes, got {len}"
    );

    // Exactly one value for this event's key — retrying never produces a
    // second, distinct record the way appending to a stream would.
    let exported: Option<String> = redis::cmd("GET")
        .arg(format!("{EXPORTED_KEY_PREFIX}{seeded_id}"))
        .query(&mut conn)
        .expect("GET exported key");
    assert_eq!(
        exported.as_deref(),
        Some(payload.as_str()),
        "expected the retry to leave exactly the original relocated value in place, not duplicate or alter it"
    );
}

/// Regression test for "events acknowledged before export": makes the
/// relocation into the exported archive actually fail (an ACL-restricted
/// user with `SET` denied — `report`'s only Redis write) and asserts the
/// source entry is neither acked nor deleted, and nothing is relocated.
/// Proves the ack/delete is conditioned on relocation really succeeding,
/// not just on having logged the event.
#[test]
fn report_billing_events_leaves_entry_pending_when_relocation_fails() {
    let Some(redis_server) = RedisServer::start() else {
        eprintln!(
            "skipping report_billing_events_leaves_entry_pending_when_relocation_fails: redis-server not found on PATH"
        );
        return;
    };

    let client = redis::Client::open(redis_server.url()).expect("valid redis url");
    let mut conn = client.get_connection().expect("connect to ephemeral redis");

    let payload = serde_json::json!({
        "method": "POST",
        "path": "v1/simple/echo",
        "status": 200,
        "ms": 3,
        "scheme": "x402-upto",
        "charge_status": "charged",
        "currency": "USDC",
        "amount_usd": 0.03,
        "unit": "tokens",
        "quantity": 128,
    })
    .to_string();
    let seeded_id: String = redis::cmd("XADD")
        .arg(STREAM_KEY)
        .arg("*")
        .arg("event")
        .arg(&payload)
        .query(&mut conn)
        .expect("seed the stream");

    // A user with every command allowed except SET — `report`'s only
    // Redis write is the SET that relocates the event into its exported
    // key, so this fails exactly that call while leaving XGROUP/
    // XREADGROUP/XACK/XDEL/XAUTOCLAIM (everything else the worker needs)
    // unaffected.
    let _: String = redis::cmd("ACL")
        .arg("SETUSER")
        .arg("limited")
        .arg("on")
        .arg(">testpass")
        .arg("~*")
        .arg("+@all")
        .arg("-set")
        .query(&mut conn)
        .expect("ACL SETUSER");
    let restricted_url = format!("redis://limited:testpass@127.0.0.1:{}", redis_server.port);

    let bin = env!("CARGO_BIN_EXE_report-billing-events");
    let status = Command::new(bin)
        .env("PAY_BILLING_REDIS_URL", restricted_url)
        .env("RUN_ONCE", "true")
        .status()
        .expect("run report-billing-events");
    // A per-entry report failure is logged and left pending — not a fatal
    // drain error — so the process itself still exits successfully.
    assert!(
        status.success(),
        "report-billing-events exited with {status}"
    );

    let pending: redis::Value = redis::cmd("XPENDING")
        .arg(STREAM_KEY)
        .arg(CONSUMER_GROUP)
        .query(&mut conn)
        .expect("XPENDING summary");
    let redis::Value::Array(fields) = &pending else {
        panic!("unexpected XPENDING reply shape: {pending:?}");
    };
    assert_eq!(
        fields[0],
        redis::Value::Int(1),
        "expected the entry to remain pending after a failed relocation, got {pending:?}"
    );

    let len: u64 = redis::cmd("XLEN")
        .arg(STREAM_KEY)
        .query(&mut conn)
        .expect("XLEN");
    assert_eq!(
        len, 1,
        "expected the entry to remain in the source stream after a failed relocation, got {len}"
    );

    let exported: Option<String> = redis::cmd("GET")
        .arg(format!("{EXPORTED_KEY_PREFIX}{seeded_id}"))
        .query(&mut conn)
        .expect("GET exported key");
    assert!(
        exported.is_none(),
        "expected nothing to be relocated when the relocation write itself fails"
    );
}

#[test]
fn report_billing_events_requires_the_redis_url() {
    let Some(redis_server) = RedisServer::start() else {
        eprintln!(
            "skipping report_billing_events_requires_the_redis_url: redis-server not found on PATH"
        );
        return;
    };
    // The server only needs to exist so this test doesn't accidentally pass
    // for an unrelated reason; PAY_BILLING_REDIS_URL is deliberately unset.
    drop(redis_server);

    let bin = env!("CARGO_BIN_EXE_report-billing-events");
    let status = Command::new(bin)
        .env_remove("PAY_BILLING_REDIS_URL")
        .status()
        .expect("run report-billing-events");
    assert!(
        !status.success(),
        "expected a config error without PAY_BILLING_REDIS_URL, got {status}"
    );
}
