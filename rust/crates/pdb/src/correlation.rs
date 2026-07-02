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

/// What the engine turns log entries into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CorrelationMode {
    /// Only 402 challenges / payment retries become flows (Payment Debugger).
    #[default]
    PaymentFlows,
    /// Every exchange becomes a flow immediately (`pay serve inference`).
    /// Payment challenge/retry *correlation* is not applied in this mode —
    /// each HTTP exchange is one flow; unifying the two models is deferred
    /// until the inference gateway grows payment gating.
    AllExchanges,
}

pub struct FlowCorrelation {
    flows: Vec<PaymentFlow>,
    /// Maps `"clientIp::path"` → index into `flows`.
    flow_index: HashMap<String, usize>,
    /// `AllExchanges` mode: maps in-flight log id → flow id (stable across
    /// ring-buffer eviction, unlike indices).
    open_exchanges: HashMap<u64, String>,
    flow_id_counter: u64,
    mode: CorrelationMode,
    tx: broadcast::Sender<SseMessage>,
}

impl FlowCorrelation {
    pub fn new(tx: broadcast::Sender<SseMessage>) -> Self {
        Self::with_mode(tx, CorrelationMode::PaymentFlows)
    }

    pub fn with_mode(tx: broadcast::Sender<SseMessage>, mode: CorrelationMode) -> Self {
        Self {
            flows: Vec::new(),
            flow_index: HashMap::new(),
            open_exchanges: HashMap::new(),
            flow_id_counter: 0,
            mode,
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

        if self.mode == CorrelationMode::AllExchanges {
            self.ingest_exchange(entry);
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

    // ── AllExchanges mode ──

    /// Open an `in-progress` flow at request time so slow requests are
    /// visible while they run. No-op in `PaymentFlows` mode.
    pub fn begin_exchange(&mut self, start: ExchangeStart) {
        if self.mode != CorrelationMode::AllExchanges || is_internal_path(&start.path) {
            return;
        }

        self.flow_id_counter += 1;
        let id = format!("flow-{}", self.flow_id_counter);
        self.open_exchanges.insert(start.id, id.clone());

        let flow = PaymentFlow {
            id,
            protocol: Protocol::Http,
            scheme: None,
            resource: start.path.clone(),
            status: FlowStatus::InProgress,
            client_ip: start.client_ip,
            started_at: start.ts.clone(),
            updated_at: start.ts.clone(),
            duration_ms: 0,
            amount: None,
            payer: None,
            session: None,
            steps: exchange_steps(&start.ts),
            events: vec![FlowEvent {
                ts: start.ts,
                message: format!("{} {}", start.method, start.path),
                detail: Some("Request forwarded upstream".into()),
            }],
            challenge_headers: None,
            payment_headers: None,
            response_headers: None,
            response_body: None,
            inference: start.inference,
        };

        self.add_flow(flow.clone());
        let _ = self.tx.send(SseMessage::FlowCreated { flow });
    }

    /// Live telemetry update for an in-flight exchange (running token counts,
    /// TTFT). Merged field-wise onto the flow's existing inference data —
    /// present incoming fields win, absent ones keep what the flow already
    /// knows (the request-time update carries provider/endpoint kind, the
    /// stream observer carries model/tokens). No-op once completed.
    pub fn update_exchange(&mut self, log_id: u64, inference: InferenceInfo) {
        let Some(flow_id) = self.open_exchanges.get(&log_id).cloned() else {
            return;
        };
        let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) else {
            return;
        };
        flow.inference = Some(match flow.inference.take() {
            Some(existing) => merge_inference(existing, inference),
            None => inference,
        });
        flow.updated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // Keep the row's duration ticking while the request runs — the UI
        // renders durationMs live on each flow-updated.
        if let Some(elapsed) = elapsed_ms(&flow.started_at, &flow.updated_at) {
            flow.duration_ms = elapsed;
        }
        let _ = self.tx.send(SseMessage::FlowUpdated { flow: flow.clone() });
    }

    /// Completion path for `AllExchanges` mode: close the in-flight flow
    /// opened by `begin_exchange`, or record a completed one-shot flow if no
    /// start was seen (e.g. traffic that bypassed the start hook).
    fn ingest_exchange(&mut self, entry: LogEntry) {
        let open_flow_id = self.open_exchanges.remove(&entry.id);

        let Some(flow_id) = open_flow_id else {
            self.create_completed_exchange(&entry);
            return;
        };
        let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) else {
            // Evicted from the ring buffer while in flight.
            self.create_completed_exchange(&entry);
            return;
        };

        let now = &entry.ts;
        flow.status = exchange_status(entry.status);
        flow.updated_at = now.clone();
        flow.duration_ms = elapsed_ms(&flow.started_at, now).unwrap_or(entry.ms);
        flow.response_headers = Some(entry.res_headers.clone());
        flow.response_body = entry.res_body.clone();
        complete_exchange_steps(flow, now);
        flow.events.push(FlowEvent {
            ts: now.clone(),
            message: format!("{} — completed in {}ms", entry.status, flow.duration_ms),
            detail: entry
                .res_body
                .as_deref()
                .map(|b| truncate(b, 2000).to_string()),
        });

        let _ = self.tx.send(SseMessage::FlowUpdated { flow: flow.clone() });
    }

    fn create_completed_exchange(&mut self, entry: &LogEntry) {
        self.flow_id_counter += 1;
        let id = format!("flow-{}", self.flow_id_counter);
        let now = &entry.ts;

        let mut flow = PaymentFlow {
            id,
            protocol: Protocol::Http,
            scheme: None,
            resource: entry.path.clone(),
            status: exchange_status(entry.status),
            client_ip: entry.client_ip.clone(),
            started_at: now.clone(),
            updated_at: now.clone(),
            duration_ms: entry.ms,
            amount: None,
            payer: None,
            session: None,
            steps: exchange_steps(now),
            events: vec![
                FlowEvent {
                    ts: now.clone(),
                    message: format!("{} {}", entry.method, entry.path),
                    detail: Some("Request forwarded upstream".into()),
                },
                FlowEvent {
                    ts: now.clone(),
                    message: format!("{} — completed in {}ms", entry.status, entry.ms),
                    detail: entry
                        .res_body
                        .as_deref()
                        .map(|b| truncate(b, 2000).to_string()),
                },
            ],
            challenge_headers: None,
            payment_headers: None,
            response_headers: Some(entry.res_headers.clone()),
            response_body: entry.res_body.clone(),
            inference: None,
        };
        complete_exchange_steps(&mut flow, now);

        self.add_flow(flow.clone());
        let _ = self.tx.send(SseMessage::FlowCreated { flow });
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
                let protocol = if is_session_challenge(entry) {
                    Protocol::Session
                } else {
                    Protocol::Mpp
                };
                return Some((protocol, Phase::Challenge));
            }
            if entry.path.starts_with("/x402/")
                // v1 (`X-PAYMENT-REQUIRED`) and v2 (`PAYMENT-REQUIRED`); header
                // keys are normalized to lowercase.
                || entry.res_headers.contains_key("x-payment-required")
                || entry.res_headers.contains_key("payment-required")
                || is_x402_body(&entry.res_body)
            {
                return Some((Protocol::X402, Phase::Challenge));
            }
            return None;
        }

        // Payment retries
        if is_session_authorization(entry.req_headers.get("authorization")) {
            return Some((Protocol::Session, Phase::Retry));
        }
        if entry.res_headers.contains_key("payment-receipt") {
            return Some((Protocol::Mpp, Phase::Retry));
        }
        // v1 (`X-PAYMENT`) and v2 (`PAYMENT-SIGNATURE` request / `PAYMENT-RESPONSE`
        // settlement); header keys are normalized to lowercase.
        if entry.req_headers.contains_key("x-payment")
            || entry.req_headers.contains_key("x-payment-response")
            || entry.req_headers.contains_key("payment-signature")
            || entry.res_headers.contains_key("payment-response")
        {
            return Some((Protocol::X402, Phase::Retry));
        }

        None
    }

    // ── Flow creation ──

    fn create_flow(&mut self, entry: &LogEntry, protocol: Protocol) {
        // Dedup re-issued challenges: clients (and the playground UI) often probe
        // an endpoint, get a 402, then probe again before paying. Without this
        // each 402 spawns its own `payment-required` row, and the eventual
        // payment merges only the most recent — orphaning the earlier orange
        // rows. Refresh the existing pending flow instead of creating a duplicate.
        if let Some(idx) = self.find_pending_flow(&entry.client_ip, &entry.path) {
            let flow = &mut self.flows[idx];
            flow.updated_at = entry.ts.clone();
            let _ = self.tx.send(SseMessage::FlowUpdated { flow: flow.clone() });
            return;
        }

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
            // Http never reaches create_flow (detect() can't return it);
            // grouped with Mpp only for exhaustiveness.
            Protocol::Mpp | Protocol::Http => format!(
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
            Protocol::Session => format!(
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

        let amount = if matches!(protocol, Protocol::Session) {
            None
        } else {
            extract_amount(entry)
        };
        let session = if matches!(protocol, Protocol::Session) {
            session_from_challenge(entry)
        } else {
            None
        };

        let flow = PaymentFlow {
            id,
            protocol,
            scheme: flow_scheme(entry, protocol, None),
            resource: entry.path.clone(),
            status: FlowStatus::PaymentRequired,
            client_ip: entry.client_ip.clone(),
            started_at: now.clone(),
            updated_at: now.clone(),
            duration_ms: 0,
            amount,
            payer: None,
            session,
            steps,
            events: vec![
                FlowEvent {
                    ts: now.clone(),
                    message: format!("{} {}", entry.method, entry.path),
                    detail: Some("Client request received".into()),
                },
                FlowEvent {
                    ts: now.clone(),
                    message: "402 Payment Gate".into(),
                    detail: Some(challenge_detail),
                },
            ],
            challenge_headers: Some(entry.res_headers.clone()),
            payment_headers: None,
            response_headers: None,
            response_body: None,
            inference: None,
        };

        self.add_flow(flow.clone());
        let _ = self.tx.send(SseMessage::FlowCreated { flow });
    }

    // ── Payment retry ──

    fn handle_retry(&mut self, entry: &LogEntry, protocol: Protocol) {
        // Exact match (IP + path), then path-only fallback.
        let idx = self.find_pending_flow(&entry.client_ip, &entry.path);

        let Some(idx) = idx else {
            if matches!(protocol, Protocol::Session) && self.merge_session_delivery(entry) {
                return;
            }
            self.create_standalone_delivery(entry, protocol);
            return;
        };

        let flow = &mut self.flows[idx];
        if flow.status != FlowStatus::PaymentRequired {
            if matches!(protocol, Protocol::Session) && self.merge_session_delivery(entry) {
                return;
            }
            self.create_standalone_delivery(entry, protocol);
            return;
        }

        // The challenge for a dual-scheme endpoint (e.g. mpp + x402) is created
        // from whichever offer header detect() saw first (www-authenticate →
        // mpp). The retry reveals the scheme the client actually used, so adopt
        // it — otherwise an x402 payment shows under the mpp challenge's label.
        let scheme = flow_scheme(entry, protocol, self.flows[idx].challenge_headers.as_ref());
        let flow = &mut self.flows[idx];
        flow.protocol = protocol;
        flow.scheme = scheme;

        let now = &entry.ts;
        let session_update = if matches!(protocol, Protocol::Session) {
            session_from_authorization(entry, flow.session.as_ref())
        } else {
            None
        };
        flow.payment_headers = Some(entry.req_headers.clone());
        flow.payer = extract_payer(&entry.req_headers);
        flow.response_headers = Some(entry.res_headers.clone());
        flow.response_body = entry.res_body.clone();
        flow.updated_at = now.clone();
        flow.duration_ms = entry.ms;

        if entry.status >= 200 && entry.status < 300 {
            flow.status = FlowStatus::ResourceDelivered;
            if session_update.is_some() {
                flow.session = session_update.clone();
            }
            let detail = match protocol {
                Protocol::Mpp | Protocol::Http => format!(
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
                Protocol::Session => session_event_detail(session_update.as_ref())
                    .unwrap_or_else(|| "session action verified".into()),
                Protocol::X402 => "x-payment-response verified".into(),
            };
            flow.events.push(FlowEvent {
                ts: now.clone(),
                message: if matches!(protocol, Protocol::Session) {
                    session_accepted_message(session_update.as_ref())
                } else {
                    "Payment accepted".into()
                },
                detail: Some(detail),
            });
            flow.events.push(FlowEvent {
                ts: now.clone(),
                message: "200 Resource Delivered".into(),
                detail: entry
                    .res_body
                    .as_deref()
                    .map(|b| truncate(b, 2000).to_string()),
            });
        } else {
            flow.status = FlowStatus::Failed;
            if let Some(mut session) = session_update {
                session.state = SessionState::Failed;
                flow.session = Some(session);
            }
            flow.events.push(FlowEvent {
                ts: now.clone(),
                message: format!("Payment retry failed with {}", entry.status),
                detail: entry
                    .res_body
                    .as_deref()
                    .map(|b| truncate(b, 2000).to_string()),
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
        let session = if matches!(protocol, Protocol::Session) {
            session_from_authorization(entry, None)
        } else {
            None
        };

        let flow = PaymentFlow {
            id,
            protocol,
            scheme: flow_scheme(entry, protocol, None),
            resource: entry.path.clone(),
            status: FlowStatus::ResourceDelivered,
            client_ip: entry.client_ip.clone(),
            started_at: now.clone(),
            updated_at: now.clone(),
            duration_ms: entry.ms,
            amount: None,
            payer: extract_payer(&entry.req_headers),
            session: session.clone(),
            steps,
            events: vec![FlowEvent {
                ts: now.clone(),
                message: if matches!(protocol, Protocol::Session) {
                    session_accepted_message(session.as_ref())
                } else {
                    format!("{} {} → {}", entry.method, entry.path, entry.status)
                },
                detail: Some(if matches!(protocol, Protocol::Session) {
                    session_event_detail(session.as_ref())
                        .unwrap_or_else(|| "Session flow completed (challenge not captured)".into())
                } else {
                    "Payment flow completed (challenge not captured)".into()
                }),
            }],
            challenge_headers: None,
            payment_headers: None,
            response_headers: Some(entry.res_headers.clone()),
            response_body: entry.res_body.clone(),
            inference: None,
        };

        self.add_flow(flow.clone());
        let _ = self.tx.send(SseMessage::FlowCreated { flow });
    }

    fn merge_session_delivery(&mut self, entry: &LogEntry) -> bool {
        let preliminary = match session_from_authorization(entry, None) {
            Some(session) => session,
            None => return false,
        };
        if !matches!(
            preliminary.action.as_deref(),
            Some("commit") | Some("voucher")
        ) {
            return false;
        }
        let Some(session_id) = preliminary.session_id.as_deref() else {
            return false;
        };

        let Some(idx) = self.flows.iter().rposition(|flow| {
            matches!(flow.protocol, Protocol::Session)
                && flow.resource == entry.path
                && flow
                    .session
                    .as_ref()
                    .and_then(|session| session.session_id.as_deref())
                    == Some(session_id)
        }) else {
            return false;
        };

        let flow = &mut self.flows[idx];
        let now = &entry.ts;
        let Some(mut session_update) = session_from_authorization(entry, flow.session.as_ref())
        else {
            return false;
        };

        flow.payment_headers = Some(entry.req_headers.clone());
        if let Some(payer) = extract_payer(&entry.req_headers) {
            flow.payer = Some(payer);
        }
        flow.response_headers = Some(entry.res_headers.clone());
        flow.response_body = entry.res_body.clone();
        flow.updated_at = now.clone();
        flow.duration_ms = elapsed_ms(&flow.started_at, now)
            .unwrap_or_else(|| flow.duration_ms.saturating_add(entry.ms));

        if entry.status >= 200 && entry.status < 300 {
            flow.status = FlowStatus::ResourceDelivered;
            flow.events.push(FlowEvent {
                ts: now.clone(),
                message: session_accepted_message(Some(&session_update)),
                detail: session_event_detail(Some(&session_update)),
            });
        } else {
            flow.status = FlowStatus::Failed;
            session_update.state = SessionState::Failed;
            flow.events.push(FlowEvent {
                ts: now.clone(),
                message: format!("Session retry failed with {}", entry.status),
                detail: entry
                    .res_body
                    .as_deref()
                    .map(|body| truncate(body, 2000).to_string()),
            });
        }

        flow.session = Some(session_update);
        update_steps(flow);
        let _ = self.tx.send(SseMessage::FlowUpdated { flow: flow.clone() });
        true
    }

    // ── Helpers ──

    /// Index of the open (`payment-required`) flow for this client+path —
    /// exact `ip::path` match first, then the most recent path-only match.
    /// Shared by retry correlation and challenge dedup.
    fn find_pending_flow(&self, client_ip: &str, path: &str) -> Option<usize> {
        self.flow_index
            .get(&flow_key(client_ip, path))
            .copied()
            .filter(|&i| self.flows[i].status == FlowStatus::PaymentRequired)
            .or_else(|| {
                self.flows
                    .iter()
                    .rposition(|f| f.resource == path && f.status == FlowStatus::PaymentRequired)
            })
    }

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

/// Field-wise merge of inference telemetry: incoming wins where present.
fn merge_inference(existing: InferenceInfo, incoming: InferenceInfo) -> InferenceInfo {
    InferenceInfo {
        provider: if incoming.provider.is_empty() {
            existing.provider
        } else {
            incoming.provider
        },
        model: incoming.model.or(existing.model),
        endpoint_kind: incoming.endpoint_kind.or(existing.endpoint_kind),
        streamed: incoming.streamed || existing.streamed,
        tokens_prompt: incoming.tokens_prompt.or(existing.tokens_prompt),
        tokens_completion: incoming.tokens_completion.or(existing.tokens_completion),
        ttft_ms: incoming.ttft_ms.or(existing.ttft_ms),
        tokens_per_sec: incoming.tokens_per_sec.or(existing.tokens_per_sec),
    }
}

/// 2xx/3xx delivered, everything else failed. (402 cannot occur on
/// passthrough inference routes in v1 — no metered endpoints.)
fn exchange_status(status: u16) -> FlowStatus {
    if (200..400).contains(&status) {
        FlowStatus::ResourceDelivered
    } else {
        FlowStatus::Failed
    }
}

/// Two-step diagram for plain exchanges: request → response.
fn exchange_steps(ts: &str) -> Vec<FlowStep> {
    vec![
        FlowStep {
            key: "request".into(),
            label: "Request".into(),
            status: StepStatus::Completed,
            ts: Some(ts.to_string()),
        },
        FlowStep {
            key: "delivery".into(),
            label: "Response".into(),
            status: StepStatus::InProgress,
            ts: None,
        },
    ]
}

fn complete_exchange_steps(flow: &mut PaymentFlow, ts: &str) {
    if let Some(step) = flow.steps.iter_mut().find(|s| s.key == "delivery") {
        step.status = match flow.status {
            FlowStatus::Failed => StepStatus::Pending,
            _ => StepStatus::Completed,
        };
        step.ts = (!matches!(flow.status, FlowStatus::Failed)).then(|| ts.to_string());
    }
}

/// Sub-scheme label for a flow, derived from the entry's headers (and the
/// stored challenge headers for a retry). Drives the `PROTOCOL:SCHEME` label.
fn flow_scheme(
    entry: &LogEntry,
    protocol: Protocol,
    challenge_headers: Option<&HashMap<String, String>>,
) -> Option<String> {
    match protocol {
        Protocol::Session => Some("session".to_string()),
        Protocol::Mpp => Some(mpp_intent(entry).unwrap_or_else(|| "charge".to_string())),
        Protocol::X402 => {
            Some(x402_scheme(entry, challenge_headers).unwrap_or_else(|| "exact".to_string()))
        }
        Protocol::Http => None,
    }
}

/// MPP intent (`charge`/`session`/`subscription`) from the challenge
/// `www-authenticate` header or the retry `authorization` credential.
fn mpp_intent(entry: &LogEntry) -> Option<String> {
    if let Some(header) = entry.res_headers.get("www-authenticate") {
        let params = parse_header_params(header.trim_start_matches("Payment").trim());
        if let Some(intent) = params.get("intent") {
            return Some(intent.clone());
        }
    }
    payment_credential_from_authorization(entry.req_headers.get("authorization"))
        .and_then(|c| value_string(c.get("challenge").and_then(|ch| ch.get("intent"))))
}

/// x402 scheme (`exact`/`upto`/`batch-settlement`) from the retry
/// `payment-signature` payload or the (this/stored) challenge `payment-required`
/// offer.
fn x402_scheme(
    entry: &LogEntry,
    challenge_headers: Option<&HashMap<String, String>>,
) -> Option<String> {
    for key in ["payment-signature", "x-payment"] {
        if let Some(value) = entry.req_headers.get(key)
            && let Some(scheme) = x402_scheme_from_payment(value)
        {
            return Some(scheme);
        }
    }
    for headers in [Some(&entry.res_headers), challenge_headers]
        .into_iter()
        .flatten()
    {
        for key in ["payment-required", "x-payment-required"] {
            if let Some(value) = headers.get(key)
                && let Some(scheme) = x402_scheme_from_required(value)
            {
                return Some(scheme);
            }
        }
    }
    None
}

/// `accepts[0].scheme` from a base64 `PAYMENT-REQUIRED` challenge envelope.
fn x402_scheme_from_required(encoded: &str) -> Option<String> {
    let json = decode_json_value(encoded)?;
    let offers = json.get("accepts").or_else(|| json.get("offers"))?;
    value_string(offers.as_array()?.first()?.get("scheme"))
}

/// Scheme from a base64 `PAYMENT-SIGNATURE` payment envelope — `accepted.scheme`
/// (canonical x402 v2), a top-level `scheme`, or `upto` inferred from a
/// payment-channel payload (`channelId`/`profile`).
fn x402_scheme_from_payment(encoded: &str) -> Option<String> {
    let json = decode_json_value(encoded)?;
    if let Some(scheme) = value_string(json.get("accepted").and_then(|a| a.get("scheme"))) {
        return Some(scheme);
    }
    if let Some(scheme) = value_string(json.get("scheme")).filter(|s| !s.is_empty()) {
        return Some(scheme);
    }
    let payload = json.get("payload")?;
    (payload.get("channelId").is_some() || payload.get("profile").is_some())
        .then(|| "upto".to_string())
}

fn is_internal_path(path: &str) -> bool {
    path.starts_with("/__402")
}

fn is_x402_body(body: &Option<String>) -> bool {
    let Some(body) = body else { return false };
    body.contains("x402Version")
}

fn build_steps(protocol: &Protocol) -> Vec<FlowStep> {
    let payment_label = match protocol {
        Protocol::Mpp | Protocol::X402 | Protocol::Http => "Paid Request",
        Protocol::Session => "Open / Voucher",
    };
    let challenge_label = match protocol {
        Protocol::Session => "402 Session Intent",
        Protocol::Mpp | Protocol::X402 | Protocol::Http => "402 Payment Gate",
    };
    vec![
        FlowStep {
            key: "request".into(),
            label: "Initial Request".into(),
            status: StepStatus::Pending,
            ts: None,
        },
        FlowStep {
            key: "challenge".into(),
            label: challenge_label.into(),
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
        // InProgress only occurs on exchange flows, which manage their own
        // 2-step diagram via `complete_exchange_steps`; treat like a fresh
        // payment flow if it ever reaches here.
        FlowStatus::PaymentRequired | FlowStatus::InProgress => 2,
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

fn elapsed_ms(start: &str, end: &str) -> Option<u64> {
    let start = chrono::DateTime::parse_from_rfc3339(start).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(end).ok()?;
    u64::try_from(
        end.timestamp_millis()
            .saturating_sub(start.timestamp_millis()),
    )
    .ok()
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
            let amount = json["amount"]
                .as_str()
                .or_else(|| json["cap"].as_str())
                .unwrap_or("0");
            let decimals = json["methodDetails"]["decimals"]
                .as_u64()
                .or_else(|| json["decimals"].as_u64())
                .unwrap_or(6);
            if let Ok(raw) = amount.parse::<u64>() {
                if raw == u64::MAX {
                    return Some("unbounded".to_string());
                }
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

/// Extract the payer's pubkey from the payment authorization header.
///
/// MPP format: `Payment <base64url-json>` where JSON contains a
/// `payload.transaction` (base64 Solana tx — first signer is the payer).
fn extract_payer(headers: &HashMap<String, String>) -> Option<String> {
    let auth = headers.get("authorization")?;
    let token = auth
        .strip_prefix("Payment ")
        .or_else(|| {
            // Also try case-insensitive match
            let lower = auth.to_lowercase();
            if lower.starts_with("payment ") {
                Some(&auth[8..])
            } else {
                None
            }
        })?
        .trim();

    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(token))
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;

    // Try payload.transaction (base64 Solana tx).
    // When feePayer is true, account_keys[0] is the server's fee payer.
    // The actual client/payer is the second signer (the one who signed
    // the token transfer). We find them by looking at which signatures
    // are non-zero (the client signs, the fee payer slot is zeroed out
    // for the server to fill in later).
    if let Some(tx_b64) = json["payload"]["transaction"].as_str() {
        let tx_bytes = base64::engine::general_purpose::STANDARD
            .decode(tx_b64)
            .ok()?;
        let tx: solana_transaction::Transaction = bincode::deserialize(&tx_bytes).ok()?;

        // Find the first account key whose signature is non-zero
        // (the client-signed key). The fee payer signature is typically
        // all zeros because the server fills it in after verification.
        let zero_sig = [0u8; 64];
        for (i, sig) in tx.signatures.iter().enumerate() {
            if sig.as_ref() != zero_sig && i < tx.message.account_keys.len() {
                return Some(tx.message.account_keys[i].to_string());
            }
        }
        // Fallback: first account key
        let pubkey = tx.message.account_keys.first()?;
        return Some(pubkey.to_string());
    }

    // Try source field (if the SDK sets it)
    json["source"].as_str().map(|s| s.to_string())
}

fn is_session_challenge(entry: &LogEntry) -> bool {
    entry
        .res_headers
        .get("www-authenticate")
        .and_then(|header| payment_challenge_from_header(header))
        .and_then(|params| params.get("intent").cloned())
        .is_some_and(|intent| intent == "session")
}

fn session_from_challenge(entry: &LogEntry) -> Option<SessionInfo> {
    let challenge = entry
        .res_headers
        .get("www-authenticate")
        .and_then(|header| payment_challenge_from_header(header))?;
    if challenge.get("intent").map(String::as_str) != Some("session") {
        return None;
    }
    let request = challenge
        .get("request")
        .and_then(|encoded| decode_json_value(encoded))?;
    let mode = request
        .get("modes")
        .and_then(|modes| modes.as_array())
        .and_then(|modes| {
            modes
                .iter()
                .filter_map(|mode| mode.as_str())
                .find(|mode| *mode == "push" || *mode == "pull")
        })
        .map(str::to_string);

    Some(SessionInfo {
        session_id: None,
        state: SessionState::Opening,
        action: None,
        mode,
        currency: value_string(request.get("currency")),
        decimals: request
            .get("decimals")
            .and_then(|v| v.as_u64())
            .and_then(|v| u8::try_from(v).ok()),
        cap: value_string(request.get("cap")),
        min_voucher_delta: value_string(request.get("minVoucherDelta")),
        deposit: None,
        approved_amount: None,
        cumulative: None,
        delta: None,
        voucher_count: None,
        authorized_signer: None,
        owner: None,
        payer: None,
        recipient: value_string(request.get("recipient")),
        splits: session_splits(request.get("splits")),
        delivery_id: None,
        opened_at: None,
        updated_at: Some(entry.ts.clone()),
    })
}

fn is_session_authorization(auth: Option<&String>) -> bool {
    payment_credential_from_authorization(auth)
        .and_then(|credential| {
            credential
                .get("challenge")
                .and_then(|challenge| challenge.get("intent"))
                .and_then(|intent| intent.as_str())
                .map(str::to_string)
        })
        .is_some_and(|intent| intent == "session")
}

fn session_from_authorization(
    entry: &LogEntry,
    previous: Option<&SessionInfo>,
) -> Option<SessionInfo> {
    let credential = payment_credential_from_authorization(entry.req_headers.get("authorization"))?;
    let challenge = credential.get("challenge")?;
    if challenge.get("intent").and_then(|v| v.as_str()) != Some("session") {
        return None;
    }
    let payload = credential.get("payload")?;
    let action = session_action(payload.get("action"));
    let receipt = parse_commit_receipt(entry.res_body.as_deref());
    let voucher_data = payload
        .get("voucher")
        .and_then(|voucher| voucher.get("data"));
    let cumulative = receipt
        .as_ref()
        .and_then(|r| r.cumulative.clone())
        .or_else(|| value_string(voucher_data.and_then(|d| d.get("cumulativeAmount"))))
        .or_else(|| value_string(voucher_data.and_then(|d| d.get("cumulative"))))
        .or_else(|| previous.and_then(|s| s.cumulative.clone()));
    let session_id = receipt
        .as_ref()
        .and_then(|r| r.session_id.clone())
        .or_else(|| value_string(payload.get("channelId")))
        .or_else(|| value_string(payload.get("tokenAccount")))
        .or_else(|| value_string(voucher_data.and_then(|d| d.get("channelId"))))
        .or_else(|| previous.and_then(|s| s.session_id.clone()));
    let has_voucher = voucher_data.is_some();
    let state = if entry.status >= 200 && entry.status < 300 {
        if action.as_deref() == Some("close") {
            SessionState::Closed
        } else {
            SessionState::Open
        }
    } else {
        SessionState::Failed
    };

    let previous_vouchers = previous.and_then(|s| s.voucher_count).unwrap_or(0);
    let action_is_open = action.as_deref() == Some("open");

    Some(SessionInfo {
        session_id,
        state,
        action,
        mode: session_mode(payload.get("mode")).or_else(|| previous.and_then(|s| s.mode.clone())),
        currency: previous.and_then(|s| s.currency.clone()),
        decimals: previous.and_then(|s| s.decimals),
        cap: previous.and_then(|s| s.cap.clone()),
        min_voucher_delta: previous.and_then(|s| s.min_voucher_delta.clone()),
        deposit: value_string(payload.get("deposit"))
            .or_else(|| previous.and_then(|s| s.deposit.clone())),
        approved_amount: value_string(payload.get("approvedAmount"))
            .or_else(|| previous.and_then(|s| s.approved_amount.clone())),
        cumulative,
        delta: receipt
            .as_ref()
            .and_then(|r| r.amount.clone())
            .or_else(|| previous.and_then(|s| s.delta.clone())),
        voucher_count: Some(previous_vouchers + if has_voucher { 1 } else { 0 }),
        authorized_signer: value_string(payload.get("authorizedSigner"))
            .or_else(|| previous.and_then(|s| s.authorized_signer.clone())),
        owner: value_string(payload.get("owner"))
            .or_else(|| previous.and_then(|s| s.owner.clone())),
        payer: value_string(payload.get("payer"))
            .or_else(|| value_string(credential.get("source")))
            .or_else(|| previous.and_then(|s| s.payer.clone())),
        recipient: previous.and_then(|s| s.recipient.clone()),
        splits: previous.map(|s| s.splits.clone()).unwrap_or_default(),
        delivery_id: receipt
            .as_ref()
            .and_then(|r| r.delivery_id.clone())
            .or_else(|| value_string(payload.get("deliveryId")))
            .or_else(|| previous.and_then(|s| s.delivery_id.clone())),
        opened_at: previous
            .and_then(|s| s.opened_at.clone())
            .or_else(|| action_is_open.then(|| entry.ts.clone())),
        updated_at: Some(entry.ts.clone()),
    })
}

fn session_accepted_message(session: Option<&SessionInfo>) -> String {
    match session.and_then(|s| s.action.as_deref()) {
        Some("open") => "Session channel opened".into(),
        Some("voucher") => "Session voucher accepted".into(),
        Some("commit") => "Session delivery committed".into(),
        Some("topUp") => "Session channel topped up".into(),
        Some("close") => "Session channel closed".into(),
        _ => "Session action accepted".into(),
    }
}

fn session_event_detail(session: Option<&SessionInfo>) -> Option<String> {
    let session = session?;
    let parts = [
        session
            .session_id
            .as_ref()
            .map(|id| format!("session={}", shorten(id))),
        session.mode.as_ref().map(|mode| format!("mode={mode}")),
        session
            .cumulative
            .as_ref()
            .map(|cumulative| format!("cumulative={cumulative}")),
        session.delta.as_ref().map(|delta| format!("delta={delta}")),
        session
            .delivery_id
            .as_ref()
            .map(|delivery| format!("delivery={}", shorten(delivery))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn payment_challenge_from_header(header: &str) -> Option<HashMap<String, String>> {
    let challenge = header
        .split("\nPayment ")
        .map(|part| {
            if part.starts_with("Payment ") {
                part.to_string()
            } else {
                format!("Payment {part}")
            }
        })
        .find(|part| part.starts_with("Payment ") && part.contains("intent=\"session\""))
        .or_else(|| header.starts_with("Payment ").then(|| header.to_string()))?;
    Some(parse_header_params(
        challenge.trim_start_matches("Payment ").trim(),
    ))
}

fn payment_credential_from_authorization(auth: Option<&String>) -> Option<serde_json::Value> {
    let auth = auth?;
    if !auth.to_ascii_lowercase().starts_with("payment ") {
        return None;
    }
    decode_json_value(auth.get(8..)?.trim())
}

fn parse_header_params(value: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for segment in value.split(',') {
        let Some((key, raw_value)) = segment.trim().split_once('=') else {
            continue;
        };
        let parsed = raw_value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        params.insert(key.trim().to_string(), parsed);
    }
    params
}

fn decode_json_value(encoded: &str) -> Option<serde_json::Value> {
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(encoded))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

struct CommitReceiptView {
    amount: Option<String>,
    cumulative: Option<String>,
    delivery_id: Option<String>,
    session_id: Option<String>,
}

fn parse_commit_receipt(body: Option<&str>) -> Option<CommitReceiptView> {
    let parsed: serde_json::Value = serde_json::from_str(body?).ok()?;
    if parsed.get("sessionId").is_none() && parsed.get("cumulative").is_none() {
        return None;
    }
    Some(CommitReceiptView {
        amount: value_string(parsed.get("amount")),
        cumulative: value_string(parsed.get("cumulative")),
        delivery_id: value_string(parsed.get("deliveryId")),
        session_id: value_string(parsed.get("sessionId")),
    })
}

fn session_splits(value: Option<&serde_json::Value>) -> Vec<SessionSplit> {
    value
        .and_then(|value| value.as_array())
        .map(|splits| {
            splits
                .iter()
                .filter_map(|split| {
                    let recipient = value_string(split.get("recipient"))?;
                    let bps = split
                        .get("bps")
                        .and_then(|value| value.as_u64())
                        .and_then(|value| u16::try_from(value).ok())?;
                    Some(SessionSplit {
                        recipient,
                        bps,
                        label: value_string(split.get("label")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn session_action(value: Option<&serde_json::Value>) -> Option<String> {
    match value.and_then(|value| value.as_str()) {
        Some("open" | "voucher" | "commit" | "topUp" | "close") => {
            value.and_then(|value| value.as_str()).map(str::to_string)
        }
        _ => None,
    }
}

fn session_mode(value: Option<&serde_json::Value>) -> Option<String> {
    match value.and_then(|value| value.as_str()) {
        Some("push" | "pull") => value.and_then(|value| value.as_str()).map(str::to_string),
        _ => None,
    }
}

fn value_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn shorten(value: &str) -> String {
    if value.len() > 16 {
        format!("{}…{}", &value[..6], &value[value.len() - 6..])
    } else {
        value.to_string()
    }
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

    fn encode_json(value: serde_json::Value) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string().as_bytes())
    }

    fn session_challenge_header() -> String {
        let request = encode_json(serde_json::json!({
            "cap": "1000000",
            "currency": "USDC",
            "decimals": 6,
            "operator": "operator",
            "recipient": "recipient",
            "minVoucherDelta": "1",
            "modes": ["push"],
            "splits": [{"recipient": "split-recipient", "bps": 1000}]
        }));
        format!(
            "Payment realm=\"test\", method=\"solana\", intent=\"session\", request=\"{request}\""
        )
    }

    fn session_authorization(payload: serde_json::Value) -> String {
        let credential = encode_json(serde_json::json!({
            "challenge": {"intent": "session"},
            "payload": payload,
            "source": "payer-wallet"
        }));
        format!("Payment {credential}")
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

        engine.ingest(make_entry("GET", "/__402/pdb/logs", 200));
        engine.ingest(make_entry("GET", "/__402/health", 200));

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
    fn session_challenge_creates_session_flow() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        let mut entry = make_entry("POST", "/v1/generate", 402);
        entry
            .res_headers
            .insert("www-authenticate".into(), session_challenge_header());

        engine.ingest(entry);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert!(matches!(flows[0].protocol, Protocol::Session));
        assert_eq!(flows[0].steps[1].label, "402 Session Intent");
        assert_eq!(flows[0].amount, None);
        let session = flows[0].session.as_ref().expect("session metadata");
        assert!(matches!(session.state, SessionState::Opening));
        assert_eq!(session.currency.as_deref(), Some("USDC"));
        assert_eq!(session.splits.len(), 1);
    }

    #[test]
    fn session_open_retry_marks_channel_open() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        let mut challenge = make_entry("POST", "/v1/generate", 402);
        challenge
            .res_headers
            .insert("www-authenticate".into(), session_challenge_header());
        engine.ingest(challenge);

        let mut retry = make_entry("POST", "/v1/generate", 200);
        retry.req_headers.insert(
            "authorization".into(),
            session_authorization(serde_json::json!({
                "action": "open",
                "mode": "push",
                "channelId": "channel-111",
                "deposit": "1000000",
                "authorizedSigner": "session-signer",
                "signature": "open-signature"
            })),
        );
        engine.ingest(retry);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
        let session = flows[0].session.as_ref().expect("session metadata");
        assert!(matches!(session.state, SessionState::Open));
        assert_eq!(session.action.as_deref(), Some("open"));
        assert_eq!(session.session_id.as_deref(), Some("channel-111"));
        assert_eq!(session.deposit.as_deref(), Some("1000000"));
    }

    #[test]
    fn session_commit_retry_merges_into_delivered_session_flow() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        let mut challenge = make_entry("POST", "/v1/generate", 402);
        challenge
            .res_headers
            .insert("www-authenticate".into(), session_challenge_header());
        engine.ingest(challenge);

        let mut open = make_entry("POST", "/v1/generate", 200);
        open.req_headers.insert(
            "authorization".into(),
            session_authorization(serde_json::json!({
                "action": "open",
                "mode": "push",
                "channelId": "channel-111",
                "deposit": "1000000",
                "authorizedSigner": "session-signer",
                "signature": "open-signature"
            })),
        );
        engine.ingest(open);

        let mut commit = make_entry("POST", "/v1/generate", 200);
        commit.req_headers.insert(
            "authorization".into(),
            session_authorization(serde_json::json!({
                "action": "commit",
                "deliveryId": "delivery-1",
                "voucher": {
                    "data": {
                        "channelId": "channel-111",
                        "cumulativeAmount": "25",
                        "expiresAt": 4102444800_u64
                    },
                    "signature": "voucher-signature"
                }
            })),
        );
        commit.res_body = Some(
            serde_json::json!({
                "deliveryId": "delivery-1",
                "sessionId": "channel-111",
                "amount": "25",
                "cumulative": "25",
                "status": "committed"
            })
            .to_string(),
        );
        engine.ingest(commit);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
        assert!(
            flows[0]
                .events
                .iter()
                .any(|event| event.message == "Session delivery committed")
        );
        let session = flows[0].session.as_ref().expect("session metadata");
        assert_eq!(session.action.as_deref(), Some("commit"));
        assert_eq!(session.session_id.as_deref(), Some("channel-111"));
        assert_eq!(session.cumulative.as_deref(), Some("25"));
        assert_eq!(session.delta.as_deref(), Some("25"));
        assert_eq!(session.delivery_id.as_deref(), Some("delivery-1"));
        assert_eq!(session.voucher_count, Some(1));
        assert_eq!(session.currency.as_deref(), Some("USDC"));
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
    fn duplicate_challenges_dedup_into_one_flow() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        // Three 402 probes for the same endpoint before paying.
        for _ in 0..3 {
            let mut e = make_entry("GET", "/api/v1/joke", 402);
            e.res_headers.insert(
                "www-authenticate".into(),
                "Payment realm=\"t\", intent=\"charge\"".into(),
            );
            engine.ingest(e);
        }

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1, "re-issued challenges must not orphan rows");
        assert_eq!(flows[0].status, FlowStatus::PaymentRequired);
        assert_eq!(flows[0].scheme.as_deref(), Some("charge"));
    }

    #[test]
    fn mpp_charge_then_pay_is_one_flow_labeled_charge() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        let mut ch = make_entry("GET", "/api/v1/joke", 402);
        ch.res_headers.insert(
            "www-authenticate".into(),
            "Payment realm=\"t\", intent=\"charge\"".into(),
        );
        engine.ingest(ch);

        let mut rt = make_entry("GET", "/api/v1/joke", 200);
        rt.req_headers
            .insert("authorization".into(), "Payment abc".into());
        rt.res_headers
            .insert("payment-receipt".into(), "receipt".into());
        engine.ingest(rt);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
        assert!(matches!(flows[0].protocol, Protocol::Mpp));
        assert_eq!(flows[0].scheme.as_deref(), Some("charge"));
    }

    #[test]
    fn retry_adopts_actual_x402_scheme_over_mpp_challenge() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        // Dual-scheme endpoint: the 402 carries both www-authenticate (mpp) and
        // payment-required (x402); detect() labels the challenge mpp.
        let mut ch = make_entry("GET", "/api/v1/fortune", 402);
        ch.res_headers.insert(
            "www-authenticate".into(),
            "Payment realm=\"t\", intent=\"charge\"".into(),
        );
        ch.res_headers.insert(
            "payment-required".into(),
            encode_json(serde_json::json!({
                "x402Version": 1,
                "accepts": [{ "scheme": "exact", "amount": "10000" }]
            })),
        );
        engine.ingest(ch);
        assert!(matches!(engine.snapshot()[0].protocol, Protocol::Mpp));

        // The client pays with x402 → the flow must adopt x402:exact, not stay mpp.
        let mut rt = make_entry("GET", "/api/v1/fortune", 200);
        rt.req_headers.insert(
            "payment-signature".into(),
            encode_json(serde_json::json!({
                "x402Version": 2,
                "payload": {},
                "accepted": { "scheme": "exact" }
            })),
        );
        rt.res_headers
            .insert("payment-response".into(), "sig".into());
        engine.ingest(rt);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1, "x402 retry merges, not standalone");
        assert!(matches!(flows[0].protocol, Protocol::X402));
        assert_eq!(flows[0].scheme.as_deref(), Some("exact"));
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
    }

    #[test]
    fn x402_upto_scheme_inferred_from_channel_payload() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        let mut ch = make_entry("POST", "/api/v1/summarize", 402);
        ch.res_headers.insert(
            "payment-required".into(),
            encode_json(serde_json::json!({
                "x402Version": 2,
                "accepts": [{ "scheme": "upto", "amount": "100000" }]
            })),
        );
        engine.ingest(ch);

        let flows = engine.snapshot();
        assert!(matches!(flows[0].protocol, Protocol::X402));
        assert_eq!(flows[0].scheme.as_deref(), Some("upto"));
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

    // ── AllExchanges mode ────────────────────────────────────────────────

    fn make_start(id: u64, method: &str, path: &str) -> ExchangeStart {
        ExchangeStart {
            id,
            ts: "2026-04-02T00:00:00.000Z".into(),
            method: method.into(),
            path: path.into(),
            client_ip: "127.0.0.1".into(),
            inference: Some(InferenceInfo {
                provider: "ollama".into(),
                model: Some("llama3.2:3b".into()),
                streamed: true,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn all_exchanges_start_then_complete() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::with_mode(tx, CorrelationMode::AllExchanges);

        engine.begin_exchange(make_start(7, "POST", "/v1/chat/completions"));

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::InProgress);
        assert!(matches!(flows[0].protocol, Protocol::Http));
        assert_eq!(
            flows[0].inference.as_ref().unwrap().provider,
            "ollama".to_string()
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            SseMessage::FlowCreated { .. }
        ));

        let mut done = make_entry("POST", "/v1/chat/completions", 200);
        done.id = 7;
        done.ts = "2026-04-02T00:00:02.500Z".into();
        engine.ingest(done);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1, "completion must not create a second flow");
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
        assert_eq!(
            flows[0].duration_ms, 2500,
            "duration from start ts, not entry.ms"
        );
        // Inference survives completion.
        assert_eq!(
            flows[0].inference.as_ref().unwrap().model.as_deref(),
            Some("llama3.2:3b")
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            SseMessage::FlowUpdated { .. }
        ));
    }

    #[test]
    fn all_exchanges_failure_marks_failed() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::with_mode(tx, CorrelationMode::AllExchanges);

        engine.begin_exchange(make_start(1, "GET", "/v1/models"));
        let mut done = make_entry("GET", "/v1/models", 500);
        done.id = 1;
        engine.ingest(done);

        assert_eq!(engine.snapshot()[0].status, FlowStatus::Failed);
    }

    #[test]
    fn all_exchanges_update_streams_telemetry() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::with_mode(tx, CorrelationMode::AllExchanges);

        engine.begin_exchange(make_start(3, "POST", "/v1/chat/completions"));
        engine.update_exchange(
            3,
            InferenceInfo {
                provider: "ollama".into(),
                model: Some("llama3.2:3b".into()),
                streamed: true,
                tokens_completion: Some(42),
                ttft_ms: Some(180),
                tokens_per_sec: Some(41.2),
                ..Default::default()
            },
        );

        let flow = &engine.snapshot()[0];
        assert_eq!(flow.status, FlowStatus::InProgress);
        let inf = flow.inference.as_ref().unwrap();
        assert_eq!(inf.tokens_completion, Some(42));
        assert_eq!(inf.ttft_ms, Some(180));

        // After completion the update is a no-op (exchange no longer open).
        let mut done = make_entry("POST", "/v1/chat/completions", 200);
        done.id = 3;
        engine.ingest(done);
        engine.update_exchange(
            3,
            InferenceInfo {
                provider: "changed".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            engine.snapshot()[0].inference.as_ref().unwrap().provider,
            "ollama"
        );
    }

    #[test]
    fn update_exchange_merges_field_wise() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::with_mode(tx, CorrelationMode::AllExchanges);

        // Request-time info: provider + endpoint kind, nothing else.
        engine.begin_exchange(ExchangeStart {
            inference: Some(InferenceInfo {
                provider: "ollama".into(),
                endpoint_kind: Some("chat".into()),
                ..Default::default()
            }),
            ..make_start(9, "POST", "/v1/chat/completions")
        });

        // Stream-observer update: usage only, empty provider.
        engine.update_exchange(
            9,
            InferenceInfo {
                provider: String::new(),
                model: Some("llama3.2:3b".into()),
                streamed: true,
                tokens_completion: Some(10),
                ..Default::default()
            },
        );

        let inf = engine.snapshot()[0].inference.clone().unwrap();
        assert_eq!(inf.provider, "ollama", "provider must survive usage merge");
        assert_eq!(inf.endpoint_kind.as_deref(), Some("chat"));
        assert_eq!(inf.model.as_deref(), Some("llama3.2:3b"));
        assert!(inf.streamed);
        assert_eq!(inf.tokens_completion, Some(10));
    }

    #[test]
    fn all_exchanges_completion_without_start_creates_completed_flow() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::with_mode(tx, CorrelationMode::AllExchanges);

        engine.ingest(make_entry("GET", "/api/tags", 200));

        let flows = engine.snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
        assert!(matches!(flows[0].protocol, Protocol::Http));
    }

    #[test]
    fn all_exchanges_internal_paths_skipped() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::with_mode(tx, CorrelationMode::AllExchanges);

        engine.begin_exchange(make_start(1, "GET", "/__402/pdb/logs"));
        engine.ingest(make_entry("GET", "/__402/health", 200));

        assert!(engine.snapshot().is_empty());
    }

    #[test]
    fn payment_flows_mode_ignores_begin_exchange() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::new(tx);

        engine.begin_exchange(make_start(1, "GET", "/v1/models"));
        assert!(engine.snapshot().is_empty());

        // And plain 200s still create nothing (debugger behavior unchanged).
        engine.ingest(make_entry("GET", "/v1/models", 200));
        assert!(engine.snapshot().is_empty());
    }

    #[test]
    fn all_exchanges_open_flow_survives_eviction_pressure() {
        let (tx, _rx) = broadcast::channel(16);
        let mut engine = FlowCorrelation::with_mode(tx, CorrelationMode::AllExchanges);

        engine.begin_exchange(make_start(0, "POST", "/v1/chat/completions"));
        // Flood the ring buffer past MAX_FLOWS so the open flow is evicted.
        for i in 1..=(MAX_FLOWS as u64 + 10) {
            let mut e = make_entry("GET", &format!("/spam/{i}"), 200);
            e.id = i;
            engine.ingest(e);
        }

        // Completing the evicted exchange must not panic or corrupt state —
        // it falls back to a fresh completed flow.
        let mut done = make_entry("POST", "/v1/chat/completions", 200);
        done.id = 0;
        engine.ingest(done);

        let flows = engine.snapshot();
        assert_eq!(flows.len(), MAX_FLOWS);
        let last = flows.last().unwrap();
        assert_eq!(last.resource, "/v1/chat/completions");
        assert_eq!(last.status, FlowStatus::ResourceDelivered);
    }

    // ── extract_payer ────────────────────────────────────────────────────

    #[test]
    fn extract_payer_returns_none_for_empty_headers() {
        let headers = HashMap::new();
        assert!(extract_payer(&headers).is_none());
    }

    #[test]
    fn extract_payer_returns_none_for_non_payment_auth() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer some-token".to_string());
        assert!(extract_payer(&headers).is_none());
    }

    #[test]
    fn extract_payer_returns_none_for_invalid_base64() {
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            "Payment !!!not-base64!!!".to_string(),
        );
        assert!(extract_payer(&headers).is_none());
    }

    #[test]
    fn extract_payer_returns_none_for_invalid_json() {
        let mut headers = HashMap::new();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"not json at all");
        headers.insert("authorization".to_string(), format!("Payment {b64}"));
        assert!(extract_payer(&headers).is_none());
    }

    #[test]
    fn extract_payer_returns_none_when_no_transaction_in_payload() {
        let mut headers = HashMap::new();
        let json = serde_json::json!({
            "challenge": {"id": "test"},
            "payload": {"signature": "abc123"}
        });
        let b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.to_string().as_bytes());
        headers.insert("authorization".to_string(), format!("Payment {b64}"));
        // Falls through to source field check, which is also absent
        assert!(extract_payer(&headers).is_none());
    }

    #[test]
    fn extract_payer_uses_source_field_as_fallback() {
        let mut headers = HashMap::new();
        let json = serde_json::json!({
            "challenge": {"id": "test"},
            "source": "MyWalletPubkey123",
            "payload": {"signature": "abc123"}
        });
        let b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.to_string().as_bytes());
        headers.insert("authorization".to_string(), format!("Payment {b64}"));
        assert_eq!(
            extract_payer(&headers).as_deref(),
            Some("MyWalletPubkey123")
        );
    }

    #[test]
    fn extract_payer_from_real_transaction() {
        // Build a minimal valid Solana transaction with a known signer.
        use solana_transaction::Transaction;

        let fee_payer = solana_pubkey::Pubkey::new_unique();
        let user_key = solana_pubkey::Pubkey::new_unique();

        // Build a message with fee_payer first, user_key second
        let instruction = solana_instruction::Instruction::new_with_bytes(
            solana_pubkey::Pubkey::new_unique(), // program
            &[],
            vec![
                solana_instruction::AccountMeta::new(fee_payer, true),
                solana_instruction::AccountMeta::new(user_key, true),
            ],
        );
        let blockhash = solana_hash::Hash::default();
        let message = solana_message::Message::new_with_blockhash(
            &[instruction],
            Some(&fee_payer),
            &blockhash,
        );

        // Create tx with placeholder signatures (fee_payer=zero, user=nonzero)
        let tx = Transaction {
            signatures: vec![
                solana_signature::Signature::default(), // fee payer: all zeros
                solana_signature::Signature::new_unique(), // user: non-zero
            ],
            message,
        };

        let tx_bytes = bincode::serialize(&tx).unwrap();
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

        let json = serde_json::json!({
            "challenge": {"id": "test"},
            "payload": {"type": "transaction", "transaction": tx_b64}
        });
        let b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.to_string().as_bytes());

        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), format!("Payment {b64}"));

        let payer = extract_payer(&headers);
        // Should return user_key (non-zero sig), not fee_payer (zero sig)
        assert_eq!(payer.as_deref(), Some(user_key.to_string().as_str()));
    }

    #[test]
    fn extract_payer_fallback_when_all_sigs_zero() {
        // If all signatures are zero, fallback to first account key
        use solana_transaction::Transaction;

        let key = solana_pubkey::Pubkey::new_unique();
        let instruction = solana_instruction::Instruction::new_with_bytes(
            solana_pubkey::Pubkey::new_unique(),
            &[],
            vec![solana_instruction::AccountMeta::new(key, true)],
        );
        let message = solana_message::Message::new_with_blockhash(
            &[instruction],
            Some(&key),
            &solana_hash::Hash::default(),
        );
        let tx = Transaction {
            signatures: vec![solana_signature::Signature::default()],
            message,
        };

        let tx_bytes = bincode::serialize(&tx).unwrap();
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

        let json = serde_json::json!({
            "challenge": {"id": "test"},
            "payload": {"type": "transaction", "transaction": tx_b64}
        });
        let b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.to_string().as_bytes());

        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), format!("Payment {b64}"));

        let payer = extract_payer(&headers);
        assert_eq!(payer.as_deref(), Some(key.to_string().as_str()));
    }
}
