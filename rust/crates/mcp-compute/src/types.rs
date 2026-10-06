use std::collections::BTreeMap;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Verified Pay identity used as the compute tenancy boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenant {
    pub payer: String,
    pub key: String,
    /// Payment channel funding resources created by this control-plane call.
    pub channel_id: String,
}

fn default_provider() -> String {
    "google-cloud-functions".to_string()
}

fn default_page_size() -> u32 {
    50
}

/// Source code supplied to a compute provider.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceInput {
    /// UTF-8 source files keyed by safe relative path.
    Inline { files: BTreeMap<String, String> },
    /// A complete zip archive encoded as standard base64.
    ZipBase64 { data: String },
}

/// Portable runtime selection. `runtime` is the provider runtime ID, such as
/// `nodejs22` or `python312`; drivers publish supported values via `providers`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct RuntimeSpec {
    pub runtime: String,
    pub entrypoint: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ResourceLimits {
    /// Maximum duration for one request. Drivers reject values over their cap.
    pub timeout_seconds: Option<u32>,
    pub memory_mb: Option<u32>,
    pub cpu: Option<f32>,
    pub min_instances: Option<u32>,
    pub max_instances: Option<u32>,
    pub concurrency: Option<u32>,
}

/// Invocation exposure. `mcp_only` permits invocation only through the
/// payer-authenticated MCP. `gateway` also enables the deployment's public,
/// stablecoin-metered wildcard hostname while the provider origin stays private.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    #[default]
    McpOnly,
    Gateway,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct AccessPolicy {
    #[serde(default)]
    pub exposure: Exposure,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct DataServiceRef {
    /// Portable data-service kind. Currently `document_store`.
    pub kind: String,
    /// Provider driver ID returned by the data MCP, such as `gcp-firestore`.
    pub driver: String,
    /// Payer-owned logical name or physical data-service ID.
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ServiceBindingSpec {
    /// Binding name used to derive `PAY_BINDING_<NAME>_*` environment keys.
    pub name: String,
    pub service: DataServiceRef,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    /// Capability lifetime. Defaults to 24 hours and is capped at 30 days.
    pub lease_seconds: Option<u32>,
}

/// Portable event trigger. Drivers reconcile these together with the
/// deployment so the same contract can target other compute providers later.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSpec {
    /// Invoke the function with POST on a Unix-cron schedule.
    Schedule {
        /// Five-field Unix cron expression, for example `*/5 * * * *`.
        cron: String,
        /// IANA timezone. Defaults to `Etc/UTC`.
        #[serde(default = "default_schedule_timezone")]
        timezone: String,
        /// Function path invoked by the scheduler. Defaults to `/`.
        #[serde(default = "default_schedule_path")]
        path: String,
        /// Reusable MPP session authorization for the paid gateway. Each run
        /// is metered against that session and stops executing when its funded
        /// channel is exhausted or expires. The MCP fills this from the
        /// current paid session and ignores caller-supplied values.
        #[serde(default)]
        authorization: Option<String>,
    },
}

fn default_schedule_timezone() -> String {
    "Etc/UTC".into()
}

fn default_schedule_path() -> String {
    "/".into()
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct DeployRequest {
    /// Driver ID returned by `providers`.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Stable deployment name. Reusing it updates the existing deployment.
    pub name: String,
    /// Provider region; omitted means the driver's configured default.
    pub region: Option<String>,
    pub source: SourceInput,
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub limits: ResourceLimits,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub access: AccessPolicy,
    /// Managed-service bindings resolved and injected service-to-service. No
    /// binding capability is returned in the MCP response.
    #[serde(default)]
    pub service_bindings: Vec<ServiceBindingSpec>,
    /// Provider-neutral triggers reconciled with the deployment.
    #[serde(default)]
    pub triggers: Vec<TriggerSpec>,
    /// Namespaced provider escape hatch. Unknown fields are rejected by the
    /// selected driver rather than silently ignored.
    #[serde(default)]
    pub provider_options: serde_json::Value,
}

impl DeployRequest {
    pub fn supply_schedule_authorization(&mut self, value: Option<&str>) {
        for trigger in &mut self.triggers {
            let TriggerSpec::Schedule { authorization, .. } = trigger;
            // The schedule and resource lease must be funded by the same
            // verified session as this deployment. Never trust an MCP payload
            // to select a different reusable bearer credential.
            *authorization = value.map(str::to_string);
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ResourceRequest {
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Provider resource name or the short deployment name.
    pub id: String,
    pub region: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ListRequest {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub region: Option<String>,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    pub page_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct OperationRequest {
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Provider operation resource name returned by `deploy` or `delete`.
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct InvokeRequest {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub id: String,
    pub region: Option<String>,
    #[serde(default = "default_http_method")]
    pub method: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: Option<serde_json::Value>,
    /// Client-side wait cap. The deployment's provider timeout remains the
    /// authoritative execution limit.
    pub wait_timeout_seconds: Option<u32>,
}

fn default_http_method() -> String {
    "POST".to_string()
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DriverCapabilities {
    pub provider: String,
    pub display_name: String,
    pub source_kinds: Vec<String>,
    pub operations: Vec<String>,
    pub max_timeout_seconds: u32,
    pub default_region: String,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    Deploying,
    Ready,
    Failed,
    Deleting,
    Unknown,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Deployment {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub region: String,
    pub state: ResourceState,
    /// Provider origin. It may require authentication and must not be treated
    /// as the future public paid-gateway URL.
    pub provider_url: Option<String>,
    /// Stable deployment identifier used as the left-most label below the
    /// configured compute gateway domain.
    pub gateway_id: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DeploymentList {
    pub deployments: Vec<Deployment>,
    pub next_page_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ComputeOperation {
    pub provider: String,
    pub id: String,
    pub state: OperationState,
    pub target_id: Option<String>,
    pub error: Option<String>,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct InvocationResult {
    pub provider: String,
    pub deployment_id: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: serde_json::Value,
    pub elapsed_ms: u64,
    /// Charge calculated by the provider driver from trusted execution usage.
    pub billed_micro_usd: u64,
}

/// Raw invocation passed through the wildcard data plane.
#[derive(Clone, Debug)]
pub struct GatewayInvokeRequest {
    pub deployment_id: String,
    pub method: String,
    pub path_and_query: String,
    pub headers: BTreeMap<String, String>,
    pub body: bytes::Bytes,
}

#[derive(Clone, Debug)]
pub struct GatewayInvocation {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: bytes::Bytes,
    pub elapsed_ms: u64,
    pub billed_micro_usd: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_session_overrides_caller_supplied_schedule_credential() {
        let mut request = DeployRequest {
            provider: default_provider(),
            name: "weather".into(),
            region: None,
            source: SourceInput::Inline {
                files: BTreeMap::new(),
            },
            runtime: RuntimeSpec {
                runtime: "nodejs22".into(),
                entrypoint: "weather".into(),
            },
            limits: ResourceLimits::default(),
            environment: BTreeMap::new(),
            access: AccessPolicy::default(),
            service_bindings: Vec::new(),
            triggers: vec![TriggerSpec::Schedule {
                cron: "*/5 * * * *".into(),
                timezone: default_schedule_timezone(),
                path: default_schedule_path(),
                authorization: Some("Payment attacker".into()),
            }],
            provider_options: serde_json::Value::Null,
        };

        request.supply_schedule_authorization(Some("Payment verified"));
        let TriggerSpec::Schedule { authorization, .. } = &request.triggers[0];
        assert_eq!(authorization.as_deref(), Some("Payment verified"));

        request.supply_schedule_authorization(None);
        let TriggerSpec::Schedule { authorization, .. } = &request.triggers[0];
        assert!(authorization.is_none());
    }
}
