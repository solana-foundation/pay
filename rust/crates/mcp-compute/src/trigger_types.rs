use std::collections::BTreeMap;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_driver() -> String {
    "google-cloud-scheduler".into()
}
fn default_timezone() -> String {
    "Etc/UTC".into()
}
fn default_path() -> String {
    "/".into()
}
fn default_page_size() -> u32 {
    50
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct TriggerTarget {
    /// Compute driver that owns the target. Currently `google-cloud-functions`.
    pub provider: String,
    /// Payer-owned logical deployment name or full provider resource name.
    pub id: String,
    pub region: Option<String>,
    #[serde(default = "default_path")]
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerConfiguration {
    /// Invoke the target on a five-field Unix-cron schedule.
    Schedule {
        /// Five-field Unix cron expression, for example `*/5 * * * *`.
        cron: String,
        /// IANA timezone. Defaults to `Etc/UTC`.
        #[serde(default = "default_timezone")]
        timezone: String,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct CreateTriggerRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    pub name: String,
    pub region: Option<String>,
    pub configuration: TriggerConfiguration,
    pub target: TriggerTarget,
    #[serde(default)]
    pub input: serde_json::Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct TriggerRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    pub id: String,
    pub region: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ListTriggersRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    pub region: Option<String>,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    pub page_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TriggerDriverCapabilities {
    pub driver: String,
    pub display_name: String,
    pub operations: Vec<String>,
    pub target_providers: Vec<String>,
    pub default_region: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Trigger {
    pub driver: String,
    pub id: String,
    pub name: String,
    pub region: String,
    pub configuration: TriggerConfiguration,
    pub state: String,
    pub target: String,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TriggerList {
    pub triggers: Vec<Trigger>,
    pub next_page_token: Option<String>,
}

/// Trusted payload embedded in a provider scheduler target. It is accepted
/// only together with the executor proof and consumes prepaid run capacity.
#[derive(Clone, Debug, Deserialize)]
pub struct ExecuteTriggerRequest {
    pub driver: String,
    pub trigger_resource: String,
    pub channel_lease: String,
    pub target_resource: String,
    pub tenant_key: String,
    pub path: String,
    #[serde(default)]
    pub input: serde_json::Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}
