use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenant {
    pub payer: String,
    pub key: String,
}

fn default_driver() -> String {
    "gcp-firestore".to_string()
}

fn default_class() -> String {
    "document/serverless".to_string()
}

fn default_page_size() -> u32 {
    50
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclaimPolicy {
    #[default]
    Delete,
    Retain,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
pub struct DocumentStoreAccess {
    /// Publish document GET operations through the paid wildcard data plane.
    /// Writes and deletes always remain payer-authenticated control operations.
    #[serde(default)]
    pub gateway_reads: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct CreateDocumentStoreRequest {
    /// Driver ID returned by `classes`. Defaults to `gcp-firestore`.
    #[serde(default = "default_driver")]
    pub driver: String,
    /// Portable data class. Firestore implements `document/serverless`.
    #[serde(default = "default_class")]
    pub class: String,
    /// Stable logical name. Reusing a name reconciles the existing claim.
    pub name: String,
    /// Provider placement region. Omit to use the driver's configured region.
    pub region: Option<String>,
    #[serde(default)]
    pub reclaim_policy: ReclaimPolicy,
    #[serde(default)]
    pub access: DocumentStoreAccess,
    /// Namespaced provider escape hatch. Unknown fields are rejected.
    #[serde(default)]
    pub driver_options: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct DocumentStoreRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    /// Logical name or physical store ID.
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ListDocumentStoresRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    pub page_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct DocumentRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    /// Logical name or physical store ID.
    pub store_id: String,
    /// Document key: 1-128 ASCII letters, digits, `_`, or `-`.
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct PutDocumentRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    /// Logical name or physical store ID.
    pub store_id: String,
    /// Document key: 1-128 ASCII letters, digits, `_`, or `-`.
    pub key: String,
    /// Arbitrary JSON payload, capped at 256 KiB after serialization.
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DataClass {
    pub driver: String,
    pub class: String,
    pub display_name: String,
    pub data_model: String,
    pub operations: Vec<String>,
    pub default_region: String,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePhase {
    Ready,
    Deleting,
    Failed,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Condition {
    pub kind: String,
    pub status: bool,
    pub reason: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DocumentStore {
    pub driver: String,
    pub class: String,
    pub id: String,
    pub name: String,
    pub region: String,
    pub phase: ResourcePhase,
    pub conditions: Vec<Condition>,
    pub reclaim_policy: ReclaimPolicy,
    pub gateway_id: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DocumentStoreList {
    pub stores: Vec<DocumentStore>,
    pub next_page_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Document {
    pub store_id: String,
    pub key: String,
    pub value: serde_json::Value,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct GatewayReadRequest {
    pub store_id: String,
    pub key: String,
}

#[derive(Clone, Debug)]
pub struct GatewayRead {
    pub document: Document,
    pub billed_micro_usd: u64,
}
