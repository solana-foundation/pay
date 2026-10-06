use std::collections::BTreeMap;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenant {
    pub payer: String,
    pub key: String,
    pub channel_id: String,
}

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
pub struct ComputeTarget {
    /// Compute driver that owns the target. Currently `google-cloud-functions`.
    pub provider: String,
    /// Payer-owned logical deployment name or full provider resource name.
    pub id: String,
    pub region: Option<String>,
    #[serde(default = "default_path")]
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct CreateJobRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    pub name: String,
    pub region: Option<String>,
    /// Five-field Unix cron expression.
    pub cron: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub target: ComputeTarget,
    #[serde(default)]
    pub input: serde_json::Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct JobRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    pub id: String,
    pub region: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ListJobsRequest {
    #[serde(default = "default_driver")]
    pub driver: String,
    pub region: Option<String>,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    pub page_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DriverCapabilities {
    pub driver: String,
    pub display_name: String,
    pub operations: Vec<String>,
    pub target_providers: Vec<String>,
    pub default_region: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Job {
    pub driver: String,
    pub id: String,
    pub name: String,
    pub region: String,
    pub cron: String,
    pub timezone: String,
    pub state: String,
    pub target: String,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct JobList {
    pub jobs: Vec<Job>,
    pub next_page_token: Option<String>,
}

/// Trusted payload embedded in a provider scheduler target. It is accepted
/// only together with the executor proof and consumes prepaid run capacity.
#[derive(Clone, Debug, Deserialize)]
pub struct ExecuteJobRequest {
    pub driver: String,
    pub job_resource: String,
    pub channel_lease: String,
    pub origin: String,
    pub path: String,
    #[serde(default)]
    pub input: serde_json::Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}
