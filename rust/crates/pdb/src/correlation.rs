//! Flow correlation engine — port of `pdb/api/correlation.ts`.
//!
//! Groups HTTP log entries into payment flows by correlating 402 challenges
//! with subsequent payment retries from the same client+path.

use std::collections::HashMap;

use base64::Engine;
use tokio::sync::broadcast;

use crate::types::*;

const FLOW_TIMEOUT_MS: u64 = 60_000;
const MAX_FLOWS: usize = 200;

#[derive(Debug, Clone, Copy)]
enum Phase {
    Challenge,
    Retry,
}

pub struct FlowCorrelation {
    flows: Vec<PaymentFlow>,
    /// Maps `"clientIp::path"` → index into `flows`.
    flow_index: HashMap<String, usize>,
    flow_id_counter: u64,
    tx: broadcast::Sender<SseMessage>,
}

impl FlowCorrelation {
    pub fn new(tx: broadcast::Sender<SseMessage>) -> Self {
        Self {
            flows: Vec::new(),
            flow_index: HashMap::new(),
            flow_id_counter: 0,
            tx,
        }
    }

    pub fn snapshot(&self) -> Vec<PaymentFlow> {
        self.flows.clone()
    }

    pub fn ingest(&mut self, entry: LogEntry) {
        if is_internal_path(&entry.path) {
            return;
        }

        let Some((protocol, phase)) = self.detect(&entry) else {
            return;
        };

        match phase {
            Phase::Challenge => self.create_flow(&entry, protocol),
            Phase::Retry => self.handle_retry(&entry, protocol),
        }
    }

    pub fn cleanup(&mut self) {
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;

        for flow in &mut self.flows {
            if flow.status != FlowStatus::PaymentRequired {
                continue;
            }
            let started = chrono::DateTime::parse_from_rfc3339(&flow.started_at)
                .map(|d| d.timestamp_millis() as u64);
            if let Ok(started_ms) = started
                && now_ms.saturating_sub(started_ms) > FLOW_TIMEOUT_MS
            {
                flow.status = FlowStatus::Failed;
                flow.updated_at =
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                flow.duration_ms = now_ms.saturating_sub(started_ms);
                flow.events.push(FlowEvent {
                    ts: flow.updated_at.clone(),
                    message: "Flow timed out — no payment received within 60s".into(),
                    detail: None,
                });
                update_steps(flow);
                let _ = self.tx.send(SseMessage::FlowUpdated { flow: flow.clone() });
            }
        }
    }

    // ── Detection ──

    fn detect(&self, entry: &LogEntry) -> Option<(Protocol, Phase)> {
        // 402 challenges
        if entry.status == 402 {
            if let Some(www_auth) = entry.res_headers.get("www-authenticate")
                && www_auth.starts_with("Payment")
            {
                return Some((Protocol::Mpp, Phase::Challenge));
            }
            if entry.path.starts_with("/x402/")
                || entry.res_headers.contains_key("x-payment-required")
                || is_x402_body(&entry.res_body)
            {
                return Some((Protocol::X402, Phase::Challenge));
            }
            return None;
        }

        // Payment retries
        if entry.res_headers.contains_key("payment-receipt") {
            return Some((Protocol::Mpp, Phase::Retry));
        }
        if entry.req_headers.contains_key("x-payment")
            || entry.req_headers.contains_key("x-payment-response")
        {
            return Some((Protocol::X402, Phase::Retry));
        }

        None
    }

    // ── Flow creation ──

    fn create_flow(&mut self, entry: &LogEntry, protocol: Protocol) {
        self.flow_id_counter += 1;
        let id = format!("flow-{}", self.flow_id_counter);
        let now = &entry.ts;

        let mut steps = build_steps(&protocol);
        steps[0].status = StepStatus::Completed;
        steps[0].ts = Some(now.clone());
        steps[1].status = StepStatus::Completed;
        steps[1].ts = Some(now.clone());
        steps[2].status = StepStatus::InProgress;

        let challenge_detail = match protocol {
            Protocol::Mpp => format!(
                "www-authenticate: {}",
                truncate(
                    entry
                        .res_headers
                        .get("www-authenticate")
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                    120
                )
            ),
            Protocol::X402 => format!(
                "x-payment-required: {}",
                truncate(
                    entry
                        .res_headers
                        .get("x-payment-required")
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                    120,
                )
            ),
        };

        let amount = extract_amount(entry);

        let flow = PaymentFlow {
            id,
            protocol,
            resource: entry.path.clone(),
            status: FlowStatus::PaymentRequired,
            client_ip: entry.client_ip.clone(),
            started_at: now.clone(),
            updated_at: now.clone(),
            duration_ms: 0,
            amount,
            steps,
            events: vec![
                FlowEvent {
                    ts: now.clone(),
                    message: format!("{} {}", entry.method, entry.path),
                    detail: Some("Client request received".into()),
                },
                FlowEvent {
                    ts: now.clone(),
                    message: "402 Payment Required".into(),
                    detail: Some(challenge_detail),
                },
            ],
            challenge_headers: Some(entry.res_headers.clone()),
            payment_headers: None,
            response_headers: None,
            response_body: None,
        };

        self.add_flow(flow.clone());
        let _ = self.tx.send(SseMessage::FlowCreated { flow });
    }

    // ── Payment retry ──

    fn handle_retry(&mut self, entry: &LogEntry, protocol: Protocol) {
        // Try exact match (IP + path), then path-only fallback
        let idx = self
            .flow_index
            .get(&flow_key(&entry.client_ip, &entry.path))
            .copied()
            .filter(|&i| self.flows[i].status == FlowStatus::PaymentRequired)
            .or_else(|| {
                self.flows.iter().rposition(|f| {
                    f.resource == entry.path && f.status == FlowStatus::PaymentRequired
                })
            });

        let Some(idx) = idx else {
            self.create_standalone_delivery(entry, protocol);
            return;
        };

        let flow = &mut self.flows[idx];
        if flow.status != FlowStatus::PaymentRequired {
            self.create_standalone_delivery(entry, protocol);
            return;
        }

        let now = &entry.ts;
        flow.payment_headers = Some(entry.req_headers.clone());
        flow.response_headers = Some(entry.res_headers.clone());
        flow.response_body = entry.res_body.clone();
        flow.updated_at = now.clone();
        flow.duration_ms = entry.ms;

        if entry.status >= 200 && entry.status < 300 {
            flow.status = FlowStatus::ResourceDelivered;
            let detail = match protocol {
                Protocol::Mpp => format!(
                    "payment-receipt: {}",
                    truncate(
                        entry
                            .res_headers
                            .get("payment-receipt")
                            .map(|s| s.as_str())
                            .unwrap_or(""),
                        120
                    )
                ),
                Protocol::X402 => "x-payment-response verified".into(),
            };
            flow.events.push(FlowEvent {
                ts: now.clone(),
                message: "Payment accepted".into(),
                detail: Some(detail),
            });
            flow.events.push(FlowEvent {
                ts: now.clone(),
                message: "200 Resource Delivered".into(),
                detail: entry
                    .res_body
                    .as_deref()
                    .map(|b| truncate(b, 200).to_string()),
            });
        } else {
            flow.status = FlowStatus::Failed;
            flow.events.push(FlowEvent {
                ts: now.clone(),
                message: format!("Payment retry failed with {}", entry.status),
                detail: entry
                    .res_body
                    .as_deref()
                    .map(|b| truncate(b, 200).to_string()),
            });
        }

        update_steps(flow);
        let _ = self.tx.send(SseMessage::FlowUpdated { flow: flow.clone() });
    }

    // ── Standalone delivery (no matching 402 found) ──

    fn create_standalone_delivery(&mut self, entry: &LogEntry, protocol: Protocol) {
        self.flow_id_counter += 1;
        let id = format!("flow-{}", self.flow_id_counter);
        let now = &entry.ts;

        let mut steps = build_steps(&protocol);
        for step in &mut steps {
            step.status = StepStatus::Completed;
            step.ts = Some(now.clone());
        }

        let flow = PaymentFlow {
            id,
            protocol,
            resource: entry.path.clone(),
            status: FlowStatus::ResourceDelivered,
            client_ip: entry.client_ip.clone(),
            started_at: now.clone(),
            updated_at: now.clone(),
            duration_ms: entry.ms,
            amount: None,
            steps,
            events: vec![FlowEvent {
                ts: now.clone(),
                message: format!("{} {} → {}", entry.method, entry.path, entry.status),
                detail: Some("Payment flow completed (challenge not captured)".into()),
            }],
            challenge_headers: None,
            payment_headers: None,
            response_headers: Some(entry.res_headers.clone()),
            response_body: entry.res_body.clone(),
        };

        self.add_flow(flow.clone());
        let _ = self.tx.send(SseMessage::FlowCreated { flow });
    }

    // ── Helpers ──

    fn add_flow(&mut self, flow: PaymentFlow) {
        let key = flow_key(&flow.client_ip, &flow.resource);
        let idx = self.flows.len();
        self.flows.push(flow);
        self.flow_index.insert(key, idx);

        if self.flows.len() > MAX_FLOWS {
            let removed = self.flows.remove(0);
            self.flow_index
                .remove(&flow_key(&removed.client_ip, &removed.resource));
            // Shift all indices down by 1
            for v in self.flow_index.values_mut() {
                *v = v.saturating_sub(1);
            }
        }
    }
}

// ── Pure helpers ──

fn flow_key(client_ip: &str, path: &str) -> String {
    format!("{client_ip}::{path}")
}

fn is_internal_path(path: &str) -> bool {
    path.starts_with("/__debugger") || path.starts_with("/__gateway")
}

fn is_x402_body(body: &Option<String>) -> bool {
    let Some(body) = body else { return false };
    body.contains("x402Version")
}

fn build_steps(protocol: &Protocol) -> Vec<FlowStep> {
    let payment_label = match protocol {
        Protocol::Mpp => "Payment Retry",
        Protocol::X402 => "Payment Retry",
    };
    vec![
        FlowStep {
            key: "request".into(),
            label: "Client Request".into(),
            status: StepStatus::Pending,
            ts: None,
        },
        FlowStep {
            key: "challenge".into(),
            label: "402 Payment Required".into(),
            status: StepStatus::Pending,
            ts: None,
        },
        FlowStep {
            key: "payment".into(),
            label: payment_label.into(),
            status: StepStatus::Pending,
            ts: None,
        },
        FlowStep {
            key: "delivery".into(),
            label: "Resource Delivered".into(),
            status: StepStatus::Pending,
            ts: None,
        },
    ]
}

fn update_steps(flow: &mut PaymentFlow) {
    let completed_count = match flow.status {
        FlowStatus::PaymentRequired => 2,
        FlowStatus::PaymentReceived => 3,
        FlowStatus::ResourceDelivered => 4,
        FlowStatus::Failed => {
            for step in &mut flow.steps {
                if matches!(step.status, StepStatus::InProgress) {
                    step.status = StepStatus::Pending;
                }
            }
            return;
        }
    };

    for (i, step) in flow.steps.iter_mut().enumerate() {
        if i < completed_count {
            step.status = StepStatus::Completed;
            if step.ts.is_none() {
                step.ts = Some(flow.updated_at.clone());
            }
        } else if i == completed_count {
            step.status = StepStatus::InProgress;
        } else {
            step.status = StepStatus::Pending;
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() > max { &s[..max] } else { s }
}

/// Extract a human-readable amount from the 402 challenge headers.
/// MPP: parses the base64 `request` param from `www-authenticate`.
/// x402: parses the JSON response body for `amount`.
fn extract_amount(entry: &LogEntry) -> Option<String> {
    // MPP: www-authenticate header contains request="<base64>"
    if let Some(www_auth) = entry.res_headers.get("www-authenticate")
        && let Some(start) = www_auth.find("request=\"")
    {
        let rest = &www_auth[start + 9..];
        if let Some(end) = rest.find('"')
            && let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&rest[..end])
                .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&rest[..end]))
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&decoded)
        {
            let amount = json["amount"].as_str().unwrap_or("0");
            let decimals = json["methodDetails"]["decimals"].as_u64().unwrap_or(6);
            if let Ok(raw) = amount.parse::<u64>() {
                let value = raw as f64 / 10f64.powi(decimals as i32);
                return Some(format!("{:.4} USDC", value));
            }
        }
    }

    // x402: response body JSON
    if let Some(body) = &entry.res_body
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(amount) = json["amount"].as_str()
    {
        return Some(amount.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(method: &str, path: &str, status: u16) -> LogEntry {
        LogEntry {
            id: 1,
            ts: "2026-04-02T00:00:00.000Z".into(),
            method: method.into(),
            path: path.into(),
            status,
            ms: 50,
            req_headers: HashMap::new(),
            res_headers: HashMap::new(),
            res_body: None,
            client_ip: "127.0.0.1".into(),
        }
    }

    #[test]
    fn challenge_creates_flow() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        let mut entry = make_entry("GET", "/mpp/quote/GOOG", 402);
        entry
            .res_headers
            .insert("www-authenticate".into(), "Payment realm=\"test\"".into());

        engine.ingest(entry);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::PaymentRequired);
        assert_eq!(flows[0].resource, "/mpp/quote/GOOG");
        assert_eq!(flows[0].events.len(), 2);
    }

    #[test]
    fn retry_completes_flow() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        // Challenge
        let mut challenge = make_entry("GET", "/mpp/quote/GOOG", 402);
        challenge
            .res_headers
            .insert("www-authenticate".into(), "Payment realm=\"test\"".into());
        engine.ingest(challenge);

        // Retry
        let mut retry = make_entry("GET", "/mpp/quote/GOOG", 200);
        retry
            .res_headers
            .insert("payment-receipt".into(), "receipt-data".into());
        engine.ingest(retry);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
    }

    #[test]
    fn internal_paths_skipped() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        engine.ingest(make_entry("GET", "/__debugger/logs", 200));
        engine.ingest(make_entry("GET", "/__gateway/health", 200));

        assert!(engine.snapshot().is_empty());
    }

    #[test]
    fn x402_challenge_detected() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        let mut entry = make_entry("GET", "/x402/joke", 402);
        entry.res_body = Some(r#"{"x402Version":"1","amount":"1000"}"#.into());
        engine.ingest(entry);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert!(matches!(flows[0].protocol, Protocol::X402));
    }

    #[test]
    fn standalone_delivery_when_no_challenge() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        let mut entry = make_entry("GET", "/mpp/quote/GOOG", 200);
        entry
            .res_headers
            .insert("payment-receipt".into(), "receipt-data".into());
        engine.ingest(entry);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
    }

    #[test]
    fn max_flows_eviction() {
        let (tx, _rx) = broadcast::channel(256);
        let mut engine = FlowCorrelation::new(tx);

        for i in 0..=MAX_FLOWS {
            let mut entry = make_entry("GET", &format!("/path/{i}"), 402);
            entry
                .res_headers
                .insert("www-authenticate".into(), "Payment realm=\"test\"".into());
            entry.client_ip = format!("10.0.0.{}", i % 256);
            engine.ingest(entry);
        }

        assert_eq!(engine.snapshot().len(), MAX_FLOWS);
    }
}
