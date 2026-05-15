//! Data types for the Payment Debugger — direct port of `pdb/api/types.ts`.

use std::collections::HashMap;

use serde::Serialize;

// ── Protocol & Status ──

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Mpp,
    X402,
    Session,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlowStatus {
    PaymentRequired,
    PaymentReceived,
    ResourceDelivered,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    Completed,
    InProgress,
    Pending,
}

// ── Flow Step (sequence diagram) ──

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowStep {
    pub key: String,
    pub label: String,
    pub status: StepStatus,
    pub ts: Option<String>,
}

// ── Flow Event (log panel) ──

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowEvent {
    pub ts: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ── Session Channel ──

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionState {
    Opening,
    Open,
    Settling,
    Closed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSplit {
    pub recipient: String,
    pub bps: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub state: SessionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_voucher_delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deposit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voucher_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorized_signer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub splits: Vec<SessionSplit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

// ── Payment Flow ──

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentFlow {
    pub id: String,
    pub protocol: Protocol,
    pub resource: String,
    pub status: FlowStatus,
    pub client_ip: String,
    pub started_at: String,
    pub updated_at: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    pub steps: Vec<FlowStep>,
    pub events: Vec<FlowEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge_headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

// ── SSE Messages ──

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SseMessage {
    #[serde(rename_all = "camelCase")]
    Init {
        viewer_ip: String,
    },
    Snapshot {
        flows: Vec<PaymentFlow>,
    },
    #[serde(rename_all = "camelCase")]
    FlowCreated {
        flow: PaymentFlow,
    },
    #[serde(rename_all = "camelCase")]
    FlowUpdated {
        flow: PaymentFlow,
    },
}

// ── Log Entry (internal, fed to correlation engine) ──

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub id: u64,
    pub ts: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ms: u64,
    pub req_headers: HashMap<String, String>,
    pub res_headers: HashMap<String, String>,
    pub res_body: Option<String>,
    pub client_ip: String,
}
