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
                let protocol = if is_session_challenge(entry) {
                    Protocol::Session
                } else {
                    Protocol::Mpp
                };
                return Some((protocol, Phase::Challenge));
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
        if is_session_authorization(entry.req_headers.get("authorization")) {
            return Some((Protocol::Session, Phase::Retry));
        }
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
    path.starts_with("/__402")
}

fn is_x402_body(body: &Option<String>) -> bool {
    let Some(body) = body else { return false };
    body.contains("x402Version")
}

fn build_steps(protocol: &Protocol) -> Vec<FlowStep> {
    let payment_label = match protocol {
        Protocol::Mpp => "Payment Retry",
        Protocol::Session => "Open / Voucher",
        Protocol::X402 => "Payment Retry",
    };
    let challenge_label = match protocol {
        Protocol::Session => "402 Session Intent",
        Protocol::Mpp | Protocol::X402 => "402 Payment Required",
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
