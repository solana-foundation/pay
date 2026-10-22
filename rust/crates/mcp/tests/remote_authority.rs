//! Loopback tests for the remote spending authority reference implementation.
//!
//! Every test spins a real HTTP server on `127.0.0.1:0` and asserts the
//! fail-closed mapping defined in `pay_mcp::authority_remote`: only an
//! unambiguous, available `{"decision":"allow"}` verdict over a 2xx response
//! may produce `Ok(AuthorityDecision::Allow)`.

use pay_mcp::authority::{
    AuthorityDecision, AuthorityUnavailable, ImmutableRequest, SpendRequest, SpendingAuthority,
};
use pay_mcp::authority_remote::RemoteHttpAuthority;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Handler = Arc<
    dyn Fn(&HashMap<String, String>, &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync,
>;

/// Minimal HTTP/1.1 loopback service: one canned handler per test run.
struct MockService {
    url: String,
    requests: Arc<Mutex<Vec<(HashMap<String, String>, Vec<u8>)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockService {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockService {
    async fn start(handler: Handler) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((listener_stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                let captured = captured.clone();
                tokio::spawn(async move {
                    let mut stream = listener_stream;
                    let mut raw = Vec::new();
                    let end = loop {
                        let mut buf = [0u8; 4096];
                        let n = match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&buf[..n]);
                        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&raw[..end]).to_string();
                    let headers = head
                        .lines()
                        .skip(1)
                        .filter_map(|line| line.split_once(':'))
                        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
                        .collect::<HashMap<_, _>>();
                    let length = headers
                        .get("content-length")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    while raw.len() - end < length {
                        let mut buf = [0u8; 4096];
                        let n = match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&buf[..n]);
                    }
                    let (status, extra_headers, body) = handler(&headers, &raw[end..]);
                    captured.lock().unwrap().push((headers, raw[end..].to_vec()));
                    let reason = match status {
                        200 => "OK",
                        302 => "Found",
                        500 => "Internal Server Error",
                        _ => "Status",
                    };
                    let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
                    for (name, value) in extra_headers {
                        response += &format!("{name}: {value}\r\n");
                    }
                    response += &format!("content-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            url: format!("http://{addr}/v1/authority/authorize"),
            requests,
            task,
        }
    }

    fn json(status: u16, body: Value) -> (u16, Vec<(String, String)>, Vec<u8>) {
        (
            status,
            vec![("content-type".to_string(), "application/json".to_string())],
            body.to_string().into_bytes(),
        )
    }

    fn received(&self) -> Vec<(HashMap<String, String>, Vec<u8>)> {
        self.requests.lock().unwrap().clone()
    }
}

fn sample_request() -> SpendRequest {
    SpendRequest {
        operation_id: "op-42".to_string(),
        attempt_id: "attempt-7".to_string(),
        caller_id: "trusted-agent-42".to_string(),
        request: ImmutableRequest {
            method: "POST".to_string(),
            url: "https://example.com/data".to_string(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Some(b"{\"hello\":\"world\"}".to_vec()),
        },
        amount_atomic: "10000".to_string(),
        asset: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
        network: "solana:mainnet".to_string(),
        execution_network: "solana:mainnet".to_string(),
        recipient: "11111111111111111111111111111111".to_string(),
        accepted: json!({
            "network": "solana:mainnet",
            "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "amount": "10000",
            "payTo": "11111111111111111111111111111111",
        }),
    }
}

async fn verdict_of(authority: &RemoteHttpAuthority) -> Result<AuthorityDecision, AuthorityUnavailable> {
    authority.authorize(sample_request()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_verdict_maps_to_allow_and_sends_the_frozen_facts() {
    let mock = MockService::start(Arc::new(|_, _| {
        MockService::json(200, json!({"decision": "allow", "unavailable": false}))
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    let verdict = verdict_of(&authority).await;
    assert!(matches!(verdict, Ok(AuthorityDecision::Allow)));

    let requests = mock.received();
    assert_eq!(requests.len(), 1);
    let (headers, body) = &requests[0];
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    let facts: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(facts["operation_id"], "op-42");
    assert_eq!(facts["attempt_id"], "attempt-7");
    assert_eq!(facts["caller_id"], "trusted-agent-42");
    assert_eq!(facts["amount_atomic"], "10000");
    assert_eq!(facts["asset"], "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    assert_eq!(facts["network"], "solana:mainnet");
    assert_eq!(facts["execution_network"], "solana:mainnet");
    assert_eq!(facts["recipient"], "11111111111111111111111111111111");
    assert_eq!(facts["request"]["method"], "POST");
    assert_eq!(facts["request"]["url"], "https://example.com/data");
    assert_eq!(
        facts["request"]["headers"],
        json!([["content-type", "application/json"]])
    );
    // `body: Option<Vec<u8>>` serializes as a byte array.
    assert!(facts["request"]["body"].is_array());
    assert_eq!(facts["request"]["body"].as_array().unwrap().len(), 17);
    assert_eq!(facts["accepted"]["payTo"], "11111111111111111111111111111111");
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_verdict_maps_to_deny() {
    let mock = MockService::start(Arc::new(|_, _| {
        MockService::json(200, json!({"decision": "deny", "reason": "over budget"}))
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    let verdict = verdict_of(&authority).await;
    assert!(matches!(verdict, Ok(AuthorityDecision::Deny)));
}

#[tokio::test(flavor = "multi_thread")]
async fn unavailable_flag_wins_over_any_decision() {
    // The UTA bridge emits `{"decision":"deny","unavailable":true}` when the
    // trust path itself failed: a host must surface unavailable, never a
    // policy denial, and never an implicit allow.
    let mock = MockService::start(Arc::new(|_, _| {
        MockService::json(200, json!({"decision": "deny", "unavailable": true}))
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    let verdict = verdict_of(&authority).await;
    assert!(matches!(verdict, Err(AuthorityUnavailable)));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_error_status_fails_closed() {
    let mock = MockService::start(Arc::new(|_, _| {
        MockService::json(500, json!({"decision": "allow"}))
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    let verdict = verdict_of(&authority).await;
    assert!(matches!(verdict, Err(AuthorityUnavailable)));
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_verdict_fails_closed() {
    let mock = MockService::start(Arc::new(|_, _| {
        (200, vec![], b"this is not json".to_vec())
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    let verdict = verdict_of(&authority).await;
    assert!(matches!(verdict, Err(AuthorityUnavailable)));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_or_missing_decision_fails_closed() {
    for body in [json!({"decision": "maybe"}), json!({})] {
        let mock = MockService::start(Arc::new(move |_, _| MockService::json(200, body.clone())))
            .await;
        let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
        let verdict = verdict_of(&authority).await;
        assert!(matches!(verdict, Err(AuthorityUnavailable)));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_endpoint_fails_closed() {
    // Claim a port, release it, and aim the authority at the now-closed port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let authority =
        RemoteHttpAuthority::new(format!("http://127.0.0.1:{port}/v1/authority/authorize")).unwrap();
    let verdict = verdict_of(&authority).await;
    assert!(matches!(verdict, Err(AuthorityUnavailable)));
}

#[tokio::test(flavor = "multi_thread")]
async fn redirects_are_not_followed() {
    let mock = MockService::start(Arc::new(|_, _| {
        (
            302,
            vec![("location".to_string(), "http://127.0.0.1:9/evil".to_string())],
            Vec::new(),
        )
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    let verdict = verdict_of(&authority).await;
    // The 302 must surface as unavailable (non-2xx verdict path), and the
    // frozen facts must not have been replayed to a second origin.
    assert!(matches!(verdict, Err(AuthorityUnavailable)));
    assert_eq!(mock.received().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn inner_request_timeout_fails_closed() {
    let mock = MockService::start(Arc::new(|_, _| {
        std::thread::sleep(Duration::from_millis(300));
        MockService::json(200, json!({"decision": "allow"}))
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url)
        .unwrap()
        .with_request_timeout(Duration::from_millis(50));
    let verdict = verdict_of(&authority).await;
    assert!(matches!(verdict, Err(AuthorityUnavailable)));
}

// Regression tests for PR #525 review findings (greptile, 2026-10-21):
// - P1: malformed `unavailable` values must fail closed (never coerce to false)
// - P2: verdict bodies are capped at MAX_VERDICT_BYTES
//
// Append to rust/crates/mcp/tests/remote_authority.rs (uses the existing
// MockService / verdict_of helpers defined in that file).

/// P1 regression: a present `unavailable` value that is not a JSON boolean
/// must fail closed. Before the fix, `as_bool()` returned `None` and
/// `unwrap_or(false)` coerced every malformed flag into "available", so
/// `{"decision":"allow","unavailable":"true"}` reached wallet approval.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_unavailable_values_fail_closed() {
    let malformed = vec![
        json!({"decision": "allow", "unavailable": "true"}),
        json!({"decision": "allow", "unavailable": "false"}),
        json!({"decision": "allow", "unavailable": null}),
        json!({"decision": "allow", "unavailable": 1}),
        json!({"decision": "allow", "unavailable": 0}),
        json!({"decision": "allow", "unavailable": {"forced": true}}),
        json!({"decision": "allow", "unavailable": ["true"]}),
    ];
    for body in malformed {
        let label = body.to_string();
        let value = body.clone();
        let mock = MockService::start(Arc::new(move |_, _| MockService::json(200, value.clone())))
            .await;
        let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
        let verdict = verdict_of(&authority).await;
        assert!(
            matches!(verdict, Err(AuthorityUnavailable)),
            "malformed unavailable flag must fail closed: {label}"
        );
    }
}

/// Sanity guard for the strict mapping: well-formed flags keep working, and
/// an explicit `true` still wins over `decision:"allow"`.
#[tokio::test(flavor = "multi_thread")]
async fn wellformed_unavailable_flags_unchanged() {
    // false + allow -> Allow (unchanged behavior)
    let mock =
        MockService::start(Arc::new(|_, _| MockService::json(200, json!({"decision": "allow", "unavailable": false}))))
            .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    assert!(matches!(verdict_of(&authority).await, Ok(AuthorityDecision::Allow)));

    // true + allow -> Unavailable (precedence unchanged)
    let mock =
        MockService::start(Arc::new(|_, _| MockService::json(200, json!({"decision": "allow", "unavailable": true}))))
            .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    assert!(matches!(verdict_of(&authority).await, Err(AuthorityUnavailable)));

    // absent flag + deny -> Deny (unchanged behavior)
    let mock = MockService::start(Arc::new(|_, _| MockService::json(200, json!({"decision": "deny"})))).await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    assert!(matches!(verdict_of(&authority).await, Ok(AuthorityDecision::Deny)));
}

/// P2 regression: a 2xx verdict larger than `MAX_VERDICT_BYTES` must fail
/// closed instead of being buffered in full.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_verdict_fails_closed() {
    // 100 KiB of padding: over the 64 KiB cap, valid JSON, decision allow.
    let body = format!(
        r#"{{"decision":"allow","pad":"{}"}}"#,
        "a".repeat(100 * 1024)
    );
    let mock = MockService::start(Arc::new(move |_, _| {
        (200, vec![], body.clone().into_bytes())
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    let verdict = verdict_of(&authority).await;
    assert!(matches!(verdict, Err(AuthorityUnavailable)));
}

/// Boundary: a verdict just under the cap is still processed normally.
#[tokio::test(flavor = "multi_thread")]
async fn verdict_under_cap_still_parsed() {
    let body = format!(
        r#"{{"decision":"allow","pad":"{}"}}"#,
        "a".repeat(32 * 1024)
    );
    let mock = MockService::start(Arc::new(move |_, _| {
        (200, vec![], body.clone().into_bytes())
    }))
    .await;
    let authority = RemoteHttpAuthority::new(&mock.url).unwrap();
    assert!(matches!(verdict_of(&authority).await, Ok(AuthorityDecision::Allow)));
}
