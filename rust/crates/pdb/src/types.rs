//! Data types for the Payment Debugger — direct port of `pdb/api/types.ts`.

use std::collections::HashMap;

use serde::Serialize;

// ── Protocol & Status ──

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Mpp,
    X402,
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
