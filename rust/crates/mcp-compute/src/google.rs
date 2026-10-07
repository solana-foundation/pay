use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use zip::write::SimpleFileOptions;

use crate::driver::{ComputeDriver, ComputeError, Result};
use crate::payment_policy::DeploymentIdentity;
use crate::payment_repository::{PolicyRepository, RETENTION_SECONDS, StoredPolicy};
use crate::types::{
    ComputeOperation, DeployRequest, Deployment, DeploymentList, DriverCapabilities, Exposure,
    GatewayInvocation, GatewayInvokeRequest, InvocationResult, InvokeRequest, ListRequest,
    OperationState, ResourceRequest, ResourceState, SourceInput, Tenant,
};

pub const DRIVER_ID: &str = "google-cloud-functions";
pub const GATEWAY_PREFIX: &str = "gcf-";
const DEFAULT_API_BASE: &str = "https://cloudfunctions.googleapis.com";
const DEFAULT_METADATA_BASE: &str = "http://metadata.google.internal/computeMetadata/v1";
const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;
const MAX_SOURCE_FILES: usize = 256;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
// Leave headroom inside the 3600-second Cloud Run timeout for function lookup,
// identity-token minting, and response forwarding.
const MAX_GATEWAY_EXECUTION_SECONDS: u32 = 3500;
const INVOCATION_USD: f64 = 0.000_000_4;
const VCPU_SECOND_USD: f64 = 0.000_012_6;
const GIB_SECOND_USD: f64 = 0.000_001_4;
const EGRESS_GIB_USD: f64 = 0.12;
const PLATFORM_MULTIPLIER: f64 = 1.20;

#[derive(Clone, Debug)]
pub struct GoogleConfig {
    pub project: String,
    pub default_region: String,
    pub api_base: String,
    pub metadata_base: String,
    pub access_token: Option<String>,
    pub identity_token: Option<String>,
    pub allow_unauthenticated_invoke: bool,
    pub function_service_account: Option<String>,
    pub build_service_account: Option<String>,
    pub gateway_domain: String,
    pub payment_policy_database: Option<String>,
}

impl GoogleConfig {
    pub fn from_env() -> Result<Self> {
        let project = env_nonempty("COMPUTE_GOOGLE_PROJECT")
            .or_else(|| env_nonempty("GOOGLE_CLOUD_PROJECT"))
            .ok_or_else(|| {
                ComputeError::Configuration(
                    "COMPUTE_GOOGLE_PROJECT or GOOGLE_CLOUD_PROJECT must be set".into(),
                )
            })?;
        Ok(Self {
            project,
            default_region: env_nonempty("COMPUTE_GOOGLE_REGION")
                .unwrap_or_else(|| "us-central1".into()),
            api_base: env_nonempty("COMPUTE_GOOGLE_API_BASE")
                .unwrap_or_else(|| DEFAULT_API_BASE.into())
                .trim_end_matches('/')
                .to_string(),
            metadata_base: env_nonempty("COMPUTE_GOOGLE_METADATA_BASE")
                .unwrap_or_else(|| DEFAULT_METADATA_BASE.into())
                .trim_end_matches('/')
                .to_string(),
            access_token: env_nonempty("COMPUTE_GOOGLE_ACCESS_TOKEN")
                .or_else(|| env_nonempty("GOOGLE_OAUTH_ACCESS_TOKEN")),
            identity_token: env_nonempty("COMPUTE_GOOGLE_ID_TOKEN"),
            allow_unauthenticated_invoke: env_flag("COMPUTE_GOOGLE_UNAUTHENTICATED_INVOKE"),
            function_service_account: env_nonempty("COMPUTE_GOOGLE_FUNCTION_SERVICE_ACCOUNT"),
            build_service_account: env_nonempty("COMPUTE_GOOGLE_BUILD_SERVICE_ACCOUNT"),
            gateway_domain: env_nonempty("COMPUTE_GATEWAY_DOMAIN")
                .unwrap_or_else(|| "cpu.gcp.gateway-402.com".into()),
            payment_policy_database: match std::env::var("COMPUTE_PAYMENT_POLICY_DATABASE") {
                Ok(value) => Some(value),
                Err(std::env::VarError::NotPresent) => None,
                Err(_) => {
                    return Err(ComputeError::Configuration(
                        "invalid COMPUTE_PAYMENT_POLICY_DATABASE".into(),
                    ));
                }
            },
        })
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_flag(name: &str) -> bool {
    env_nonempty(name).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes"))
}

#[derive(Clone)]
pub struct GoogleCloudFunctionsDriver {
    config: Arc<GoogleConfig>,
    client: reqwest::Client,
    token: Arc<RwLock<Option<CachedToken>>>,
    policies: Option<PolicyRepository>,
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    refresh_after: Instant,
}

#[derive(Debug, Deserialize)]
struct MetadataToken {
    access_token: String,
    expires_in: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoogleOptions {
    description: Option<String>,
    ingress_settings: Option<String>,
    labels: BTreeMap<String, String>,
}

impl GoogleCloudFunctionsDriver {
    pub(crate) fn policy_repository(&self) -> Option<&PolicyRepository> {
        self.policies.as_ref()
    }

    fn validate_policy_identity(&self, identity: &DeploymentIdentity) -> Result<()> {
        let name = identity
            .resource_name
            .rsplit('/')
            .next()
            .unwrap_or_default();
        validate_gateway_function_name(name)?;
        let expected = format!(
            "projects/{}/locations/{}/functions/{name}",
            self.config.project, self.config.default_region,
        );
        if identity.resource_name != expected
            || identity.owner_key.len() != 16
            || !identity
                .owner_key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || tenant_label_from_resource(&expected)? != identity.owner_key
            || identity.hostname != format!("{name}.{}", self.config.gateway_domain)
        {
            return Err(ComputeError::Configuration(
                "payment policy identity is outside this driver".into(),
            ));
        }
        let created = parse_creation_time(&identity.created_at)?;
        if created > chrono::Utc::now() {
            return Err(ComputeError::Configuration(
                "payment policy incarnation is in the future".into(),
            ));
        }
        Ok(())
    }

    fn policy_identity(&self, resource: &str, value: &Value) -> Result<DeploymentIdentity> {
        let owner_key = tenant_label_from_resource(resource)?;
        if value.get("name").and_then(Value::as_str) != Some(resource)
            || value.pointer("/labels/pay-tenant").and_then(Value::as_str)
                != Some(owner_key.as_str())
            || value.pointer("/labels/managed-by").and_then(Value::as_str) != Some("mcp-compute")
        {
            return Err(ComputeError::Provider(
                "deployment ownership metadata is invalid".into(),
            ));
        }
        let identity = DeploymentIdentity {
            owner_key,
            resource_name: resource.into(),
            created_at: value
                .get("createTime")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ComputeError::Provider("deployment creation identity is missing".into())
                })?
                .into(),
            hostname: format!(
                "{}.{}",
                resource.rsplit('/').next().unwrap_or_default(),
                self.config.gateway_domain
            ),
        };
        self.validate_policy_identity(&identity)?;
        Ok(identity)
    }

    /// Only a 404 or a verified *newer* incarnation proves this identity absent.
    /// DELETING and all other states of the same incarnation remain present.
    async fn policy_incarnation_absent(&self, identity: &DeploymentIdentity) -> Result<bool> {
        self.validate_policy_identity(identity)?;
        let (status, value) = tokio::time::timeout(
            Duration::from_secs(15),
            self.send_json(
                Method::GET,
                format!("{}/v2/{}", self.config.api_base, identity.resource_name),
                None,
            ),
        )
        .await
        .map_err(|_| ComputeError::Provider("provider verification timed out".into()))??;
        if status == StatusCode::NOT_FOUND {
            return Ok(true);
        }
        if !status.is_success() {
            return Err(json_provider_error(status, &value));
        }
        let current = self.policy_identity(&identity.resource_name, &value)?;
        let current_time = parse_creation_time(&current.created_at)?;
        let stored_time = parse_creation_time(&identity.created_at)?;
        if current_time < stored_time {
            return Err(ComputeError::Provider(
                "provider incarnation predates stored policy".into(),
            ));
        }
        Ok(current_time > stored_time)
    }

    async fn retire_policy(&self, identity: &DeploymentIdentity) -> Result<()> {
        let Some(repository) = &self.policies else {
            return Ok(());
        };
        self.validate_policy_identity(identity)?;
        let token = self.access_token().await?;
        let stored = repository.load(&token, identity).await?;
        if stored.as_ref().is_some_and(|stored| stored.retired) {
            return Ok(());
        }
        let next = match &stored {
            Some(stored) => stored.retire()?,
            None => StoredPolicy {
                deployment: identity.clone(),
                policy: None,
                retired: true,
                absent_since: None,
                update_time: String::new(),
            },
        };
        // A conflict aborts deletion. The retry reads and retires the winning
        // policy rather than allowing a concurrent set to escape retirement.
        repository.write(&token, stored.as_ref(), &next).await
    }

    async fn reconcile_policy(
        &self,
        stored: &StoredPolicy,
        dry_run: bool,
        now: u64,
    ) -> Result<bool> {
        let repository = self.policies.as_ref().ok_or_else(|| {
            ComputeError::Configuration("payment policy repository is not configured".into())
        })?;
        if !self.policy_incarnation_absent(&stored.deployment).await? {
            // Never purge a pending retirement, even if malformed external
            // metadata has incorrectly assigned it an old retention timestamp.
            return Ok(false);
        }
        let expired = stored.retired
            && stored.absent_since.is_some_and(|since| {
                now.checked_sub(since)
                    .is_some_and(|elapsed| elapsed >= RETENTION_SECONDS)
            });
        if stored.retired && stored.absent_since.is_some() && !expired {
            return Ok(false);
        }
        tracing::info!(dry_run, action = if expired { "purge" } else { "retire" }, policy_key = %PolicyRepository::key(&stored.deployment), "orphan payment policy candidate");
        if dry_run {
            return Ok(true);
        }
        let token = self.access_token().await?;
        if expired {
            // Recheck after scan/retention selection; the DELETE also compares
            // the document updateTime, never a caller-supplied version.
            if !self.policy_incarnation_absent(&stored.deployment).await? {
                return Ok(false);
            }
            repository.purge(&token, stored).await?;
        } else {
            let mut next = stored.retire()?;
            next.absent_since = Some(now);
            repository.write(&token, Some(stored), &next).await?;
        }
        Ok(true)
    }

    async fn delete_resource(
        &self,
        tenant: &Tenant,
        request: ResourceRequest,
        expected_cleanup: Option<(&str, &str)>,
    ) -> Result<ComputeOperation> {
        let resource = self.resource_name(tenant, &request.id, request.region.as_deref())?;
        let url = format!("{}/v2/{resource}", self.config.api_base);
        let (status, existing) = self.send_json(Method::GET, url.clone(), None).await?;
        if status == StatusCode::NOT_FOUND {
            // Periodic reconciliation retires metadata even when provider
            // metadata (including createTime) no longer exists.
            return Ok(deleted_operation(&resource));
        }
        if !status.is_success() {
            return Err(json_provider_error(status, &existing));
        }
        require_tenant_label(tenant, &existing)?;
        if let Some((lease, created_at)) = expected_cleanup
            && (existing
                .pointer("/labels/pay-channel")
                .and_then(Value::as_str)
                != Some(lease)
                || existing.get("createTime").and_then(Value::as_str) != Some(created_at)
                || existing
                    .pointer("/labels/managed-by")
                    .and_then(Value::as_str)
                    != Some("mcp-compute")
                || existing.get("name").and_then(Value::as_str) != Some(resource.as_str()))
        {
            return Err(ComputeError::Provider(
                "deployment incarnation or channel lease changed during cleanup".into(),
            ));
        }
        let identity = if self.policies.is_some()
            && resource.starts_with(&format!(
                "projects/{}/locations/{}/functions/",
                self.config.project, self.config.default_region
            )) {
            let identity = self.policy_identity(&resource, &existing)?;
            self.retire_policy(&identity).await?;
            Some(identity)
        } else {
            None
        };
        // Google Functions DELETE has no incarnation precondition. This GET
        // revalidation fences list-to-delete races, but cannot make provider
        // deletion atomic with a concurrent out-of-band recreation.
        let (status, value) = self.send_json(Method::DELETE, url, None).await?;
        let operation = if status == StatusCode::NOT_FOUND {
            deleted_operation(&resource)
        } else if status.is_success() {
            normalize_operation(value)?
        } else {
            return Err(json_provider_error(status, &value));
        };
        if let (Some(identity), Some(repository)) = (identity, &self.policies) {
            // Retirement above is mandatory before deletion. After acceptance,
            // cleanup is best effort: reconciliation can retry it, but the
            // caller must retain the operation ID needed to track deletion.
            let cleanup: Result<()> = async {
                let token = self.access_token().await?;
                if let Some(stored) = repository.load(&token, &identity).await? {
                    self.reconcile_policy(&stored, false, policy_now()?).await?;
                }
                Ok(())
            }
            .await;
            if cleanup.is_err() {
                tracing::warn!(
                    operation_id = %operation.id,
                    "accepted compute deletion deferred policy cleanup to reconciliation"
                );
            }
        }
        Ok(operation)
    }

    /// Resolve a public selector through authenticated provider metadata. The
    /// hostname supplies a lookup key, never an ownership assertion.
    pub async fn payment_deployment(
        &self,
        hostname: &str,
        path: Option<&str>,
    ) -> Result<crate::payment_policy::DeploymentIdentity> {
        let suffix = format!(".{}", self.config.gateway_domain);
        let id = hostname.strip_suffix(&suffix).ok_or_else(|| {
            ComputeError::InvalidRequest("hostname is outside the compute gateway domain".into())
        })?;
        validate_gateway_function_name(id)?;
        let resource = format!(
            "projects/{}/locations/{}/functions/{id}",
            self.config.project, self.config.default_region
        );
        let value = self
            .require_json(
                Method::GET,
                format!("{}/v2/{resource}", self.config.api_base),
                None,
            )
            .await?;
        let owner_key = tenant_label_from_resource(&resource)?;
        if owner_key.len() != 16
            || !owner_key.bytes().all(|byte| byte.is_ascii_hexdigit())
            || value.get("name").and_then(Value::as_str) != Some(resource.as_str())
            || value.pointer("/labels/pay-tenant").and_then(Value::as_str) != Some(&owner_key)
            || value.pointer("/labels/managed-by").and_then(Value::as_str) != Some("mcp-compute")
            || value
                .pointer("/labels/pay-exposure")
                .and_then(Value::as_str)
                != Some("gateway")
            || value.get("state").and_then(Value::as_str) != Some("ACTIVE")
        {
            return Err(ComputeError::InvalidRequest(
                "deployment is not an active owned gateway".into(),
            ));
        }
        if let Some(path) = path {
            let paths = value
                .pointer("/serviceConfig/environmentVariables/PAY_INTERNAL_PUBLIC_PATHS")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ComputeError::InvalidRequest("deployment has no public paths".into())
                })?;
            let paths: Vec<String> = serde_json::from_str(paths)?;
            validate_public_paths(&paths)?;
            if !path_is_public(&paths, path) {
                return Err(ComputeError::InvalidRequest("path is not published".into()));
            }
        }
        let created_at = value
            .get("createTime")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ComputeError::Provider("deployment creation identity is missing".into())
            })?;
        Ok(crate::payment_policy::DeploymentIdentity {
            owner_key,
            resource_name: resource,
            created_at: created_at.into(),
            hostname: hostname.into(),
        })
    }

    pub fn new(config: GoogleConfig) -> Result<Self> {
        validate_segment("project", &config.project)?;
        validate_segment("default region", &config.default_region)?;
        let policies = config.payment_policy_database.as_ref().map(|database| {
            if !(4..=63).contains(&database.len())
                || !database.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                || !database.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
                || !database.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(ComputeError::Configuration(
                    "COMPUTE_PAYMENT_POLICY_DATABASE must name an explicit database".into(),
                ));
            }
            PolicyRepository::new(format!(
                "https://firestore.googleapis.com/v1/projects/{}/databases/{database}/documents/pay_compute_payment_policies",
                config.project,
            ))
        }).transpose()?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            config: Arc::new(config),
            client,
            token: Arc::new(RwLock::new(None)),
            policies,
        })
    }

    pub(crate) async fn access_token(&self) -> Result<String> {
        if let Some(token) = &self.config.access_token {
            return Ok(token.clone());
        }
        if let Some(cached) = self.token.read().await.as_ref()
            && Instant::now() < cached.refresh_after
        {
            return Ok(cached.value.clone());
        }
        let url = format!(
            "{}/instance/service-accounts/default/token",
            self.config.metadata_base
        );
        let response = self
            .client
            .get(url)
            .timeout(METADATA_REQUEST_TIMEOUT)
            .header("Metadata-Flavor", "Google")
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        if !status.is_success() {
            return Err(provider_error(status, &body));
        }
        let token: MetadataToken = serde_json::from_slice(&body)?;
        let refresh_after =
            Instant::now() + Duration::from_secs(token.expires_in.saturating_sub(60).max(1));
        *self.token.write().await = Some(CachedToken {
            value: token.access_token.clone(),
            refresh_after,
        });
        Ok(token.access_token)
    }

    pub(crate) async fn identity_token(&self, audience: &str) -> Result<Option<String>> {
        if self.config.allow_unauthenticated_invoke {
            return Ok(None);
        }
        if let Some(token) = &self.config.identity_token {
            return Ok(Some(token.clone()));
        }
        let url = format!(
            "{}/instance/service-accounts/default/identity",
            self.config.metadata_base
        );
        let response = self
            .client
            .get(url)
            .timeout(METADATA_REQUEST_TIMEOUT)
            .query(&[("audience", audience), ("format", "full")])
            .header("Metadata-Flavor", "Google")
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        if !status.is_success() {
            return Err(provider_error(status, &body));
        }
        let token = String::from_utf8(body.to_vec())
            .map_err(|_| ComputeError::Provider("metadata identity token was not UTF-8".into()))?;
        Ok(Some(token))
    }

    async fn send_json(
        &self,
        method: Method,
        url: String,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value)> {
        let token = self.access_token().await?;
        let mut request = self.client.request(method, url).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        Ok((status, value))
    }

    async fn require_json(
        &self,
        method: Method,
        url: String,
        body: Option<&Value>,
    ) -> Result<Value> {
        let (status, value) = self.send_json(method, url, body).await?;
        if !status.is_success() {
            return Err(json_provider_error(status, &value));
        }
        Ok(value)
    }

    fn region<'a>(&'a self, requested: Option<&'a str>) -> Result<&'a str> {
        let region = requested.unwrap_or(&self.config.default_region);
        validate_segment("region", region)?;
        Ok(region)
    }

    fn resource_name(&self, tenant: &Tenant, id: &str, region: Option<&str>) -> Result<String> {
        if id.starts_with("projects/") {
            let parts: Vec<_> = id.split('/').collect();
            if parts.len() != 6
                || parts[0] != "projects"
                || parts[2] != "locations"
                || parts[4] != "functions"
            {
                return Err(ComputeError::InvalidRequest(
                    "invalid Google function resource name".into(),
                ));
            }
            if parts[1] != self.config.project {
                return Err(ComputeError::InvalidRequest(
                    "function belongs to a different Google project".into(),
                ));
            }
            validate_segment("region", parts[3])?;
            validate_owned_function_name(tenant, parts[5])?;
            return Ok(id.to_string());
        }
        validate_function_name(id)?;
        let physical_name = physical_function_name(tenant, id)?;
        Ok(format!(
            "projects/{}/locations/{}/functions/{physical_name}",
            self.config.project,
            self.region(region)?
        ))
    }

    async fn source_storage(&self, request: &DeployRequest, region: &str) -> Result<Value> {
        let archive = source_zip(&request.source)?;
        let parent = format!("projects/{}/locations/{region}", self.config.project);
        let upload = self
            .require_json(
                Method::POST,
                format!(
                    "{}/v2/{parent}/functions:generateUploadUrl",
                    self.config.api_base
                ),
                Some(&json!({ "environment": "GEN_2" })),
            )
            .await?;
        let upload_url = upload
            .get("uploadUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ComputeError::Provider("generateUploadUrl response omitted uploadUrl".into())
            })?;
        let storage_source = upload.get("storageSource").cloned().ok_or_else(|| {
            ComputeError::Provider("generateUploadUrl response omitted storageSource".into())
        })?;
        let response = self
            .client
            .put(upload_url)
            .header("content-type", "application/zip")
            .body(archive)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(provider_error(status, &bytes));
        }
        Ok(storage_source)
    }

    fn function_body(
        &self,
        tenant: &Tenant,
        request: &DeployRequest,
        resource_name: &str,
        storage_source: Value,
        options: GoogleOptions,
    ) -> Result<Value> {
        let timeout = request.limits.timeout_seconds.unwrap_or(300);
        if !(1..=MAX_GATEWAY_EXECUTION_SECONDS).contains(&timeout) {
            return Err(ComputeError::InvalidRequest(format!(
                "Google gateway functions require timeout_seconds between 1 and {MAX_GATEWAY_EXECUTION_SECONDS}"
            )));
        }
        let exposure = match request.access.exposure {
            Exposure::McpOnly => "mcp-only",
            Exposure::Gateway => "gateway",
        };
        let ingress = options
            .ingress_settings
            .unwrap_or_else(|| "ALLOW_ALL".into());
        if !matches!(
            ingress.as_str(),
            "ALLOW_ALL" | "ALLOW_INTERNAL_ONLY" | "ALLOW_INTERNAL_AND_GCLB"
        ) {
            return Err(ComputeError::InvalidRequest(format!(
                "unsupported Google ingress setting `{ingress}`"
            )));
        }
        let mut labels = options.labels;
        labels.insert("managed-by".into(), "mcp-compute".into());
        labels.insert(
            "pay-tenant".into(),
            tenant_label_from_resource(resource_name)?,
        );
        labels.insert("pay-name".into(), request.name.clone());
        labels.insert("pay-exposure".into(), exposure.into());
        labels.insert("pay-channel".into(), channel_lease_key(&tenant.channel_id)?);
        if request.limits.min_instances.unwrap_or(0) != 0 {
            return Err(ComputeError::InvalidRequest(
                "min_instances is not supported yet because idle instance cost cannot be attributed to an invocation; use 0 or omit it".into(),
            ));
        }
        let mut environment = request.environment.clone();
        if environment.contains_key("PAY_INTERNAL_PUBLIC_PATHS") {
            return Err(ComputeError::InvalidRequest(
                "PAY_INTERNAL_PUBLIC_PATHS is reserved by the compute gateway".into(),
            ));
        }
        let public_paths = validate_public_paths(&request.access.public_paths)?;
        environment.insert(
            "PAY_INTERNAL_PUBLIC_PATHS".into(),
            serde_json::to_string(&public_paths)?,
        );
        let mut service = serde_json::Map::new();
        service.insert("timeoutSeconds".into(), json!(timeout));
        service.insert("ingressSettings".into(), json!(ingress));
        service.insert("environmentVariables".into(), json!(environment));
        if let Some(memory) = request.limits.memory_mb {
            if !(128..=32768).contains(&memory) {
                return Err(ComputeError::InvalidRequest(
                    "memory_mb must be between 128 and 32768".into(),
                ));
            }
            service.insert("availableMemory".into(), json!(format!("{memory}M")));
        }
        if let Some(cpu) = request.limits.cpu {
            if !cpu.is_finite() || cpu <= 0.0 || cpu > 8.0 {
                return Err(ComputeError::InvalidRequest(
                    "cpu must be greater than 0 and at most 8".into(),
                ));
            }
            service.insert("availableCpu".into(), json!(cpu.to_string()));
        }
        for (field, value) in [
            ("minInstanceCount", request.limits.min_instances),
            ("maxInstanceCount", request.limits.max_instances),
            ("maxInstanceRequestConcurrency", request.limits.concurrency),
        ] {
            if let Some(value) = value {
                service.insert(field.into(), json!(value));
            }
        }
        if let Some(email) = &self.config.function_service_account {
            service.insert("serviceAccountEmail".into(), json!(email));
        }
        let mut build = serde_json::Map::new();
        build.insert("runtime".into(), json!(request.runtime.runtime));
        build.insert("entryPoint".into(), json!(request.runtime.entrypoint));
        build.insert("source".into(), json!({ "storageSource": storage_source }));
        if let Some(email) = &self.config.build_service_account {
            build.insert(
                "serviceAccount".into(),
                json!(format!(
                    "projects/{}/serviceAccounts/{email}",
                    self.config.project
                )),
            );
        }
        Ok(json!({
            "name": resource_name,
            "description": options.description.unwrap_or_else(|| "Managed by Pay compute MCP".into()),
            "environment": "GEN_2",
            "labels": labels,
            "buildConfig": Value::Object(build),
            "serviceConfig": Value::Object(service)
        }))
    }

    async fn invoke_origin(
        &self,
        deployment: &Deployment,
        method: &str,
        path_and_query: &str,
        headers: BTreeMap<String, String>,
        body: bytes::Bytes,
        wait_seconds: u32,
    ) -> Result<GatewayInvocation> {
        let origin = deployment.provider_url.as_deref().ok_or_else(|| {
            ComputeError::Provider("function is not ready and has no invocation URL".into())
        })?;
        let mut url = url::Url::parse(origin).map_err(|error| {
            ComputeError::Provider(format!("provider returned an invalid URL: {error}"))
        })?;
        apply_relative_path(&mut url, path_and_query)?;
        let mut outgoing = self.client.request(parse_method(method)?, url);
        if let Some(token) = self.identity_token(origin).await? {
            outgoing = outgoing.bearer_auth(token);
        }
        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "authorization" | "host" | "cookie" | "content-length" | "x-pay-gcp-cpu-microusd"
            ) || lower.starts_with("payment-")
                || lower.starts_with("x-payment")
            {
                continue;
            }
            outgoing = outgoing.header(name, value);
        }
        if !body.is_empty() {
            outgoing = outgoing.body(body);
        }
        let started = Instant::now();
        let response = outgoing
            .timeout(Duration::from_secs(u64::from(wait_seconds)))
            .send()
            .await?;
        let status = response.status().as_u16();
        let headers: BTreeMap<String, String> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                let name = name.as_str();
                matches!(
                    name,
                    "content-type" | "content-language" | "etag" | "cache-control"
                )
                .then(|| {
                    value
                        .to_str()
                        .ok()
                        .map(|v| (name.to_string(), v.to_string()))
                })
                .flatten()
            })
            .collect();
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ComputeError::Provider(format!(
                "function response exceeded {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ComputeError::Provider(format!(
                    "function response exceeded {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        let billed_micro_usd = invocation_price_micro_usd(deployment, elapsed_ms, body.len());
        Ok(GatewayInvocation {
            status,
            headers,
            body: body.into(),
            elapsed_ms,
            billed_micro_usd,
        })
    }
}

#[async_trait]
impl ComputeDriver for GoogleCloudFunctionsDriver {
    fn id(&self) -> &'static str {
        DRIVER_ID
    }

    fn gateway_prefix(&self) -> &'static str {
        GATEWAY_PREFIX
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            provider: DRIVER_ID.into(),
            display_name: "Google Cloud Run functions (Cloud Functions v2 API)".into(),
            source_kinds: vec!["inline".into(), "zip_base64".into()],
            operations: vec![
                "deploy".into(),
                "get".into(),
                "list".into(),
                "invoke".into(),
                "delete".into(),
                "operation_status".into(),
            ],
            max_timeout_seconds: MAX_GATEWAY_EXECUTION_SECONDS,
            default_region: self.config.default_region.clone(),
            notes: vec![
                "Deployments are second-generation HTTP functions.".into(),
                "Attach prepaid schedule triggers with this compute MCP after deployment succeeds.".into(),
                "Provider origins stay private; invocation is performed by this MCP.".into(),
                "Public gateway invocation supports exact x402 and MPP payments; timeout_seconds controls execution lifetime.".into(),
            ],
        }
    }

    async fn deploy(&self, tenant: &Tenant, request: DeployRequest) -> Result<ComputeOperation> {
        validate_function_name(&request.name)?;
        validate_runtime(&request.runtime.runtime, &request.runtime.entrypoint)?;
        let region = self.region(request.region.as_deref())?.to_string();
        if request.access.exposure == Exposure::Gateway && region != self.config.default_region {
            return Err(ComputeError::InvalidRequest(format!(
                "gateway deployments must use the configured gateway region `{}`",
                self.config.default_region
            )));
        }
        let resource_name = self.resource_name(tenant, &request.name, Some(&region))?;
        let physical_name = resource_name.rsplit('/').next().ok_or_else(|| {
            ComputeError::Configuration("generated function resource omitted its name".into())
        })?;
        let options: GoogleOptions = if request.provider_options.is_null() {
            GoogleOptions::default()
        } else {
            serde_json::from_value(request.provider_options.clone()).map_err(|error| {
                ComputeError::InvalidRequest(format!("invalid Google provider_options: {error}"))
            })?
        };
        let (status, current) = self
            .send_json(
                Method::GET,
                format!("{}/v2/{resource_name}", self.config.api_base),
                None,
            )
            .await?;
        let exists = match status {
            StatusCode::OK => {
                require_tenant_label(tenant, &current)?;
                require_channel_lease(&tenant.channel_id, &current)?;
                true
            }
            StatusCode::NOT_FOUND => false,
            _ => return Err(json_provider_error(status, &current)),
        };
        let storage = self.source_storage(&request, &region).await?;
        let function = self.function_body(tenant, &request, &resource_name, storage, options)?;
        let operation = if exists {
            self.require_json(
                Method::PATCH,
                format!(
                    "{}/v2/{resource_name}?updateMask=buildConfig,serviceConfig,description,labels",
                    self.config.api_base
                ),
                Some(&function),
            )
            .await?
        } else {
            let parent = format!("projects/{}/locations/{region}", self.config.project);
            self.require_json(
                Method::POST,
                format!(
                    "{}/v2/{parent}/functions?functionId={}",
                    self.config.api_base, physical_name
                ),
                Some(&function),
            )
            .await?
        };
        normalize_operation(operation)
    }

    async fn get(&self, tenant: &Tenant, request: ResourceRequest) -> Result<Deployment> {
        let resource = self.resource_name(tenant, &request.id, request.region.as_deref())?;
        let (status, value) = self
            .send_json(
                Method::GET,
                format!("{}/v2/{resource}", self.config.api_base),
                None,
            )
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Err(ComputeError::ResourceNotFound);
        }
        if !status.is_success() {
            return Err(json_provider_error(status, &value));
        }
        require_tenant_label(tenant, &value)?;
        normalize_deployment(value)
    }

    async fn list(&self, tenant: &Tenant, request: ListRequest) -> Result<DeploymentList> {
        if request.page_size == 0 || request.page_size > 1000 {
            return Err(ComputeError::InvalidRequest(
                "page_size must be between 1 and 1000".into(),
            ));
        }
        let region = self.region(request.region.as_deref())?;
        let parent = format!("projects/{}/locations/{region}", self.config.project);
        let mut url = url::Url::parse(&format!("{}/v2/{parent}/functions", self.config.api_base))
            .map_err(|error| ComputeError::Configuration(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("pageSize", &request.page_size.to_string());
        if let Some(token) = request.page_token {
            url.query_pairs_mut().append_pair("pageToken", &token);
        }
        let value = self.require_json(Method::GET, url.into(), None).await?;
        let deployments = value
            .get("functions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|value| has_tenant_label(tenant, value))
            .cloned()
            .map(normalize_deployment)
            .collect::<Result<Vec<_>>>()?;
        Ok(DeploymentList {
            deployments,
            next_page_token: value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    async fn delete(&self, tenant: &Tenant, request: ResourceRequest) -> Result<ComputeOperation> {
        self.delete_resource(tenant, request, None).await
    }

    async fn operation(&self, tenant: &Tenant, id: &str) -> Result<ComputeOperation> {
        if !id.starts_with(&format!("projects/{}/locations/", self.config.project))
            || !id.contains("/operations/")
        {
            return Err(ComputeError::InvalidRequest(
                "operation ID is outside the configured Google project".into(),
            ));
        }
        let value = self
            .require_json(
                Method::GET,
                format!("{}/v2/{id}", self.config.api_base),
                None,
            )
            .await?;
        require_operation_tenant(tenant, &value)?;
        normalize_operation(value)
    }

    async fn invoke(&self, tenant: &Tenant, request: InvokeRequest) -> Result<InvocationResult> {
        let deployment = self
            .get(
                tenant,
                ResourceRequest {
                    provider: request.provider.clone(),
                    id: request.id,
                    region: request.region,
                },
            )
            .await?;
        let body = request
            .body
            .map(|value| serde_json::to_vec(&value))
            .transpose()?;
        let wait = request
            .wait_timeout_seconds
            .unwrap_or(MAX_GATEWAY_EXECUTION_SECONDS);
        if !(1..=MAX_GATEWAY_EXECUTION_SECONDS).contains(&wait) {
            return Err(ComputeError::InvalidRequest(format!(
                "wait_timeout_seconds must be between 1 and {MAX_GATEWAY_EXECUTION_SECONDS}"
            )));
        }
        let raw = self
            .invoke_origin(
                &deployment,
                &request.method,
                &request.path,
                request.headers,
                body.map(bytes::Bytes::from).unwrap_or_default(),
                wait,
            )
            .await?;
        let content_type = raw.headers.get("content-type").cloned().unwrap_or_default();
        let bytes = raw.body;
        let body = if content_type.contains("application/json") {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({ "text": String::from_utf8_lossy(&bytes) }))
        } else if let Ok(text) = String::from_utf8(bytes.to_vec()) {
            json!({ "text": text })
        } else {
            json!({ "base64": base64::engine::general_purpose::STANDARD.encode(&bytes) })
        };
        Ok(InvocationResult {
            provider: DRIVER_ID.into(),
            deployment_id: deployment.id,
            status: raw.status,
            headers: raw.headers,
            body,
            elapsed_ms: raw.elapsed_ms,
            billed_micro_usd: raw.billed_micro_usd,
        })
    }

    async fn invoke_gateway(&self, request: GatewayInvokeRequest) -> Result<GatewayInvocation> {
        validate_gateway_function_name(&request.deployment_id)?;
        let resource = format!(
            "projects/{}/locations/{}/functions/{}",
            self.config.project, self.config.default_region, request.deployment_id
        );
        let value = self
            .require_json(
                Method::GET,
                format!("{}/v2/{resource}", self.config.api_base),
                None,
            )
            .await?;
        if value.pointer("/labels/managed-by").and_then(Value::as_str) != Some("mcp-compute") {
            return Err(ComputeError::Provider(
                "deployment is not managed by compute MCP".into(),
            ));
        }
        if value
            .pointer("/labels/pay-exposure")
            .and_then(Value::as_str)
            != Some("gateway")
        {
            return Err(ComputeError::Provider(
                "deployment owner has not enabled paid gateway exposure".into(),
            ));
        }
        let public_paths = value
            .pointer("/serviceConfig/environmentVariables/PAY_INTERNAL_PUBLIC_PATHS")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ComputeError::Provider("deployment has no explicit paid gateway path policy".into())
            })?;
        let public_paths: Vec<String> = serde_json::from_str(public_paths).map_err(|_| {
            ComputeError::Provider("deployment paid gateway path policy is invalid".into())
        })?;
        if !path_is_public(&public_paths, &request.path_and_query) {
            return Err(ComputeError::InvalidRequest(
                "requested path is not published by the deployment owner".into(),
            ));
        }
        let deployment = normalize_deployment(value)?;
        self.invoke_origin(
            &deployment,
            &request.method,
            &request.path_and_query,
            request.headers,
            request.body,
            MAX_GATEWAY_EXECUTION_SECONDS,
        )
        .await
    }

    async fn cleanup_channel(&self, channel_id: &str) -> Result<usize> {
        let lease_key = channel_lease_key(channel_id)?;
        let mut page_token: Option<String> = None;
        let mut resources = Vec::new();
        loop {
            let mut url = url::Url::parse(&format!(
                "{}/v2/projects/{}/locations/-/functions",
                self.config.api_base, self.config.project
            ))
            .map_err(|error| ComputeError::Configuration(error.to_string()))?;
            url.query_pairs_mut().append_pair("pageSize", "1000");
            if let Some(token) = page_token.as_deref() {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let value = self.require_json(Method::GET, url.into(), None).await?;
            if value
                .get("unreachable")
                .and_then(Value::as_array)
                .is_some_and(|locations| !locations.is_empty())
            {
                return Err(ComputeError::Provider(
                    "Google could not scan every function location during channel cleanup".into(),
                ));
            }
            for function in value
                .get("functions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|function| {
                    function
                        .pointer("/labels/pay-channel")
                        .and_then(Value::as_str)
                        == Some(lease_key.as_str())
                        && function
                            .pointer("/labels/managed-by")
                            .and_then(Value::as_str)
                            == Some("mcp-compute")
                })
            {
                let resource = function
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ComputeError::Provider("listed function omitted resource name".into())
                    })?;
                let created_at = function
                    .get("createTime")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ComputeError::Provider("listed function omitted creation identity".into())
                    })?;
                parse_creation_time(created_at)?;
                resources.push((resource.to_owned(), created_at.to_owned()));
            }
            page_token = value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|token| !token.is_empty());
            if page_token.is_none() {
                break;
            }
        }

        for (resource, created_at) in &resources {
            let tenant = Tenant {
                payer: String::new(),
                key: tenant_label_from_resource(resource)?,
                channel_id: channel_id.to_string(),
            };
            self.delete_resource(
                &tenant,
                ResourceRequest {
                    provider: DRIVER_ID.into(),
                    id: resource.clone(),
                    region: None,
                },
                Some((&lease_key, created_at)),
            )
            .await?;
        }
        Ok(resources.len())
    }

    async fn reconcile_orphans(&self, dry_run: bool) -> Result<usize> {
        let repository = self.policies.as_ref().ok_or_else(|| {
            ComputeError::Configuration(
                "COMPUTE_PAYMENT_POLICY_DATABASE is required for reconciliation".into(),
            )
        })?;
        let started = Instant::now();
        let scope = format!(
            "{}\0{}",
            self.config.default_region, self.config.gateway_domain
        );
        let token = self.access_token().await?;
        let mut checkpoint = repository.checkpoint(&token, &scope).await?;
        let mut candidates = 0;
        let mut failures = 0;
        for _ in 0..10 {
            let token = self.access_token().await?;
            let page = repository
                .page(&token, checkpoint.page_token.as_deref())
                .await;
            let (documents, next_page) = match page {
                Ok(page) => page,
                Err(error) => {
                    // Tokens may expire between scheduled runs. Reset progress
                    // conservatively, surface the error, and restart next run.
                    if !dry_run && checkpoint.page_token.is_some() {
                        repository
                            .advance_checkpoint(&token, &scope, &mut checkpoint, None)
                            .await?;
                    }
                    return Err(error);
                }
            };
            for document in documents {
                let result = tokio::time::timeout(Duration::from_secs(10), async {
                    let stored = PolicyRepository::decode(&document)?;
                    repository.validate_name(&document, &stored)?;
                    self.validate_policy_identity(&stored.deployment)?;
                    self.reconcile_policy(&stored, dry_run, policy_now()?).await
                })
                .await;
                match result {
                    Ok(Ok(candidate)) => candidates += usize::from(candidate),
                    Ok(Err(error)) => {
                        failures += 1;
                        tracing::warn!(%error, dry_run, "payment policy reconciliation failed; will retry on next scan");
                    }
                    Err(_) => {
                        failures += 1;
                        tracing::warn!(
                            dry_run,
                            "payment policy reconciliation timed out; will retry on next scan"
                        );
                    }
                }
            }
            let complete = next_page.is_none();
            if dry_run {
                checkpoint.page_token = next_page;
            } else {
                repository
                    .advance_checkpoint(&token, &scope, &mut checkpoint, next_page)
                    .await?;
            }
            if complete || started.elapsed() >= Duration::from_secs(120) {
                break;
            }
        }
        tracing::info!(
            dry_run,
            candidates,
            failures,
            has_more = checkpoint.page_token.is_some(),
            "payment policy reconciliation batch finished"
        );
        if failures != 0 {
            return Err(ComputeError::Provider(format!(
                "{failures} payment policy reconciliation records failed; retry required"
            )));
        }
        Ok(candidates)
    }
}

fn policy_now() -> Result<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|now| now.as_secs())
        .map_err(|_| ComputeError::Configuration("system clock precedes Unix epoch".into()))
}

fn parse_creation_time(value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&chrono::Utc))
        .map_err(|_| ComputeError::Configuration("invalid deployment creation identity".into()))
}

fn deleted_operation(resource: &str) -> ComputeOperation {
    ComputeOperation {
        provider: DRIVER_ID.into(),
        id: resource.into(),
        state: OperationState::Succeeded,
        target_id: Some(resource.into()),
        error: None,
        metadata: json!({"alreadyAbsent": true}),
    }
}

fn source_zip(source: &SourceInput) -> Result<Vec<u8>> {
    match source {
        SourceInput::ZipBase64 { data } => {
            if data.len() > MAX_SOURCE_BYTES * 2 {
                return Err(ComputeError::InvalidRequest(
                    "base64 source archive is too large".into(),
                ));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|error| {
                    ComputeError::InvalidRequest(format!("invalid base64 source archive: {error}"))
                })?;
            if bytes.len() > MAX_SOURCE_BYTES {
                return Err(ComputeError::InvalidRequest(format!(
                    "source archive exceeds {MAX_SOURCE_BYTES} bytes"
                )));
            }
            if !bytes.starts_with(b"PK\x03\x04") {
                return Err(ComputeError::InvalidRequest(
                    "zip_base64 source is not a ZIP archive".into(),
                ));
            }
            Ok(bytes)
        }
        SourceInput::Inline { files } => {
            if files.is_empty() || files.len() > MAX_SOURCE_FILES {
                return Err(ComputeError::InvalidRequest(format!(
                    "inline source must contain 1 to {MAX_SOURCE_FILES} files"
                )));
            }
            let total: usize = files.values().map(String::len).sum();
            if total > MAX_SOURCE_BYTES {
                return Err(ComputeError::InvalidRequest(format!(
                    "inline source exceeds {MAX_SOURCE_BYTES} bytes"
                )));
            }
            let cursor = Cursor::new(Vec::new());
            let mut archive = zip::ZipWriter::new(cursor);
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o644);
            for (path, content) in files {
                validate_source_path(path)?;
                archive.start_file(path, options).map_err(|error| {
                    ComputeError::Provider(format!("failed to package source: {error}"))
                })?;
                archive.write_all(content.as_bytes()).map_err(|error| {
                    ComputeError::Provider(format!("failed to package source: {error}"))
                })?;
            }
            let cursor = archive.finish().map_err(|error| {
                ComputeError::Provider(format!("failed to finish source archive: {error}"))
            })?;
            Ok(cursor.into_inner())
        }
    }
}

fn validate_source_path(path: &str) -> Result<()> {
    let candidate = std::path::Path::new(path);
    if path.is_empty() || path.len() > 240 || candidate.is_absolute() || path.contains('\0') {
        return Err(ComputeError::InvalidRequest(format!(
            "unsafe source path `{path}`"
        )));
    }
    if candidate
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(ComputeError::InvalidRequest(format!(
            "unsafe source path `{path}`"
        )));
    }
    Ok(())
}

fn validate_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(ComputeError::InvalidRequest(format!(
            "invalid {label} `{value}`"
        )));
    }
    Ok(())
}

fn validate_function_name(name: &str) -> Result<()> {
    if !(4..=42).contains(&name.len())
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ComputeError::InvalidRequest(
            "function name must be 4-42 lowercase letters, digits, or hyphens; start with a letter and end with a letter or digit (the remaining Google name length is reserved for tenant isolation)".into(),
        ));
    }
    Ok(())
}

fn physical_function_name(tenant: &Tenant, logical_name: &str) -> Result<String> {
    validate_function_name(logical_name)?;
    if tenant.key.len() != 16 || !tenant.key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ComputeError::Configuration(
            "tenant key must be 16 hexadecimal characters".into(),
        ));
    }
    Ok(format!(
        "{GATEWAY_PREFIX}{}-{logical_name}",
        tenant.key.to_ascii_lowercase()
    ))
}

fn validate_gateway_function_name(name: &str) -> Result<()> {
    if !name.starts_with(GATEWAY_PREFIX)
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ComputeError::InvalidRequest(
            "invalid Google compute gateway deployment ID".into(),
        ));
    }
    Ok(())
}

fn validate_owned_function_name(tenant: &Tenant, name: &str) -> Result<()> {
    validate_gateway_function_name(name)?;
    let prefix = format!("{GATEWAY_PREFIX}{}-", tenant.key);
    if !name.starts_with(&prefix) {
        return Err(ComputeError::InvalidRequest(
            "deployment belongs to a different payer".into(),
        ));
    }
    Ok(())
}

fn channel_lease_key(channel_id: &str) -> Result<String> {
    if channel_id.is_empty() {
        return Err(ComputeError::InvalidRequest(
            "resource deployment requires a verified payment channel".into(),
        ));
    }
    let digest = Sha256::digest(channel_id.as_bytes());
    Ok(digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn tenant_label_from_resource(resource: &str) -> Result<String> {
    let name = resource.rsplit('/').next().unwrap_or_default();
    validate_gateway_function_name(name)?;
    name.strip_prefix(GATEWAY_PREFIX)
        .and_then(|suffix| suffix.split_once('-'))
        .map(|(tenant, _)| tenant.to_string())
        .ok_or_else(|| {
            ComputeError::InvalidRequest("deployment name omitted tenant namespace".into())
        })
}

fn has_tenant_label(tenant: &Tenant, value: &Value) -> bool {
    value.pointer("/labels/pay-tenant").and_then(Value::as_str) == Some(tenant.key.as_str())
}

fn require_tenant_label(tenant: &Tenant, value: &Value) -> Result<()> {
    if has_tenant_label(tenant, value) {
        Ok(())
    } else {
        Err(ComputeError::Provider(
            "deployment was not found for this payer".into(),
        ))
    }
}

fn require_channel_lease(channel_id: &str, value: &Value) -> Result<()> {
    let expected = channel_lease_key(channel_id)?;
    if let Some(existing) = value.pointer("/labels/pay-channel").and_then(Value::as_str)
        && existing != expected
    {
        return Err(ComputeError::InvalidRequest(
            "deployment is leased to a different payment channel; use a new name".into(),
        ));
    }
    Ok(())
}

fn require_operation_tenant(tenant: &Tenant, value: &Value) -> Result<()> {
    let target = value
        .pointer("/metadata/target")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/response/name").and_then(Value::as_str))
        .ok_or_else(|| ComputeError::Provider("operation omitted its target resource".into()))?;
    let name = target.rsplit('/').next().unwrap_or_default();
    validate_owned_function_name(tenant, name)
        .map_err(|_| ComputeError::Provider("operation was not found for this payer".into()))
}

fn validate_runtime(runtime: &str, entrypoint: &str) -> Result<()> {
    for (label, value) in [("runtime", runtime), ("entrypoint", entrypoint)] {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            return Err(ComputeError::InvalidRequest(format!(
                "invalid {label} `{value}`"
            )));
        }
    }
    Ok(())
}

fn validate_public_paths(paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() || paths.len() > 32 {
        return Err(ComputeError::InvalidRequest(
            "gateway exposure requires between 1 and 32 public paths".into(),
        ));
    }
    let mut normalized = paths.to_vec();
    normalized.sort();
    normalized.dedup();
    for path in &normalized {
        if !safe_public_path(path) {
            return Err(ComputeError::InvalidRequest(format!(
                "public gateway path `{path}` must be an exact safe absolute path"
            )));
        }
    }
    Ok(normalized)
}

fn path_is_public(public_paths: &[String], path_and_query: &str) -> bool {
    let requested_path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(path, _)| path);
    safe_public_path(requested_path) && public_paths.iter().any(|path| path == requested_path)
}

fn safe_public_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= 256
        // Reject alternate encodings instead of interpreting them differently
        // from the provider URL parser or the workload's HTTP framework.
        && !path.bytes().any(|byte| byte.is_ascii_control())
        && !path.contains(['?', '#', '%', '\\'])
        && !path.split('/').any(|segment| matches!(segment, "." | ".."))
        && path != "/internal"
        && !path.starts_with("/internal/")
        && path != "/__402"
        && !path.starts_with("/__402/")
}

fn normalize_deployment(value: Value) -> Result<Deployment> {
    let id = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ComputeError::Provider("function response omitted name".into()))?
        .to_string();
    let parts: Vec<_> = id.split('/').collect();
    let physical_name = parts.last().copied().unwrap_or(&id).to_string();
    let name = value
        .pointer("/labels/pay-name")
        .and_then(Value::as_str)
        .unwrap_or(&physical_name)
        .to_string();
    let region = parts.get(3).copied().unwrap_or("unknown").to_string();
    let state = match value.get("state").and_then(Value::as_str).unwrap_or("") {
        "ACTIVE" => ResourceState::Ready,
        "DEPLOYING" => ResourceState::Deploying,
        "DELETING" => ResourceState::Deleting,
        "FAILED" | "UNKNOWN" => ResourceState::Failed,
        _ => ResourceState::Unknown,
    };
    let provider_url = value
        .get("serviceConfig")
        .and_then(|v| v.get("uri"))
        .and_then(Value::as_str)
        .or_else(|| value.get("url").and_then(Value::as_str))
        .map(str::to_string);
    let metadata = json!({
        "runtime": value.get("buildConfig").and_then(|v| v.get("runtime")),
        "entrypoint": value.get("buildConfig").and_then(|v| v.get("entryPoint")),
        "timeout_seconds": value.get("serviceConfig").and_then(|v| v.get("timeoutSeconds")),
        "available_memory": value.get("serviceConfig").and_then(|v| v.get("availableMemory")),
        "available_cpu": value.get("serviceConfig").and_then(|v| v.get("availableCpu")),
        "labels": value.get("labels")
    });
    let gateway_id = (value
        .pointer("/labels/pay-exposure")
        .and_then(Value::as_str)
        == Some("gateway"))
    .then_some(physical_name);
    Ok(Deployment {
        provider: DRIVER_ID.into(),
        id,
        name,
        region,
        state,
        provider_url,
        gateway_id,
        created_at: value
            .get("createTime")
            .and_then(Value::as_str)
            .map(str::to_string),
        updated_at: value
            .get("updateTime")
            .and_then(Value::as_str)
            .map(str::to_string),
        metadata,
    })
}

fn invocation_price_micro_usd(
    deployment: &Deployment,
    elapsed_ms: u64,
    response_bytes: usize,
) -> u64 {
    let cpu = deployment
        .metadata
        .get("available_cpu")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(1.0);
    let memory_gib = deployment
        .metadata
        .get("available_memory")
        .and_then(Value::as_str)
        .and_then(parse_memory_gib)
        .unwrap_or(0.5);
    let rounded_ms = elapsed_ms.max(1).div_ceil(100) * 100;
    let seconds = rounded_ms as f64 / 1000.0;
    let egress_gib = response_bytes as f64 / 1024_f64.powi(3);
    let provider_usd = INVOCATION_USD
        + cpu * seconds * VCPU_SECOND_USD
        + memory_gib * seconds * GIB_SECOND_USD
        + egress_gib * EGRESS_GIB_USD;
    (provider_usd * PLATFORM_MULTIPLIER * 1_000_000.0)
        .ceil()
        .max(1.0) as u64
}

fn parse_memory_gib(value: &str) -> Option<f64> {
    if let Some(mib) = value.strip_suffix('M') {
        return mib.parse::<f64>().ok().map(|amount| amount / 1024.0);
    }
    if let Some(gib) = value.strip_suffix("Gi") {
        return gib.parse::<f64>().ok();
    }
    if let Some(gb) = value.strip_suffix('G') {
        return gb.parse::<f64>().ok();
    }
    value
        .parse::<f64>()
        .ok()
        .map(|bytes| bytes / 1024_f64.powi(3))
}

fn normalize_operation(value: Value) -> Result<ComputeOperation> {
    let id = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ComputeError::Provider("operation response omitted name".into()))?
        .to_string();
    let done = value.get("done").and_then(Value::as_bool).unwrap_or(false);
    let error = value
        .get("error")
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let state = if error.is_some() {
        OperationState::Failed
    } else if done {
        OperationState::Succeeded
    } else {
        OperationState::Pending
    };
    let target_id = value
        .get("response")
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("metadata")
                .and_then(|v| v.get("target"))
                .and_then(Value::as_str)
        })
        .map(str::to_string);
    Ok(ComputeOperation {
        provider: DRIVER_ID.into(),
        id,
        state,
        target_id,
        error,
        metadata: value.get("metadata").cloned().unwrap_or(Value::Null),
    })
}

fn parse_method(value: &str) -> Result<Method> {
    match value.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        "PUT" => Ok(Method::PUT),
        "PATCH" => Ok(Method::PATCH),
        "DELETE" => Ok(Method::DELETE),
        "HEAD" => Ok(Method::HEAD),
        _ => Err(ComputeError::InvalidRequest(format!(
            "unsupported invocation method `{value}`"
        ))),
    }
}

fn apply_relative_path(url: &mut url::Url, requested: &str) -> Result<()> {
    if requested.starts_with("//") || requested.contains('#') {
        return Err(ComputeError::InvalidRequest(
            "invocation path must be relative and cannot contain a fragment".into(),
        ));
    }
    let (path, query) = requested
        .split_once('?')
        .map_or((requested, None), |(p, q)| (p, Some(q)));
    if path.split('/').any(|segment| segment == "..") {
        return Err(ComputeError::InvalidRequest(
            "invocation path cannot contain `..`".into(),
        ));
    }
    let base = url.path().trim_end_matches('/').to_string();
    let suffix = path.trim_start_matches('/');
    let joined = if suffix.is_empty() {
        base
    } else {
        format!("{base}/{suffix}")
    };
    url.set_path(&joined);
    url.set_query(query);
    Ok(())
}

fn provider_error(status: StatusCode, body: &[u8]) -> ComputeError {
    let text = String::from_utf8_lossy(&body[..body.len().min(4096)]);
    ComputeError::Provider(format!("Google API returned {status}: {text}"))
}

fn json_provider_error(status: StatusCode, value: &Value) -> ComputeError {
    let message = value
        .get("error")
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("unknown provider error");
    ComputeError::Provider(format!("Google API returned {status}: {message}"))
}

#[cfg(test)]
#[path = "google_policy_tests.rs"]
mod policy_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::response::Response;

    fn json_response(status: StatusCode, value: Value) -> Response {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn cleanup_deletes_only_functions_leased_to_the_channel() {
        let lease_key = channel_lease_key("verifiedchannel").unwrap();
        let app = Router::new().fallback(move |request: Request<Body>| {
            let lease_key = lease_key.clone();
            async move {
                let path = request.uri().path();
                match (request.method().as_str(), path) {
                    ("GET", "/v2/projects/project/locations/-/functions") => json_response(
                        StatusCode::OK,
                        json!({
                            "functions": [
                                {
                                    "name": "projects/project/locations/us-central1/functions/gcf-0123456789abcdef-weather",
                                    "createTime": "2025-01-01T00:00:00Z",
                                    "labels": { "managed-by": "mcp-compute", "pay-channel": lease_key }
                                },
                                {
                                    "name": "projects/project/locations/us-central1/functions/gcf-fedcba9876543210-other",
                                    "labels": { "managed-by": "mcp-compute", "pay-channel": "other" }
                                }
                            ]
                        }),
                    ),
                    ("GET", "/v2/projects/project/locations/us-central1/functions/gcf-0123456789abcdef-weather") => json_response(
                        StatusCode::OK,
                        json!({
                            "name": "projects/project/locations/us-central1/functions/gcf-0123456789abcdef-weather",
                            "createTime": "2025-01-01T00:00:00Z",
                            "labels": { "managed-by": "mcp-compute", "pay-tenant": "0123456789abcdef", "pay-channel": lease_key }
                        }),
                    ),
                    ("DELETE", "/v2/projects/project/locations/us-central1/functions/gcf-0123456789abcdef-weather") => json_response(
                        StatusCode::OK,
                        json!({ "name": "projects/project/locations/us-central1/operations/delete-weather" }),
                    ),
                    _ => json_response(StatusCode::NOT_FOUND, json!({ "path": path })),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let driver = GoogleCloudFunctionsDriver::new(GoogleConfig {
            project: "project".into(),
            default_region: "us-central1".into(),
            api_base: base.clone(),
            metadata_base: "http://metadata.invalid".into(),
            access_token: Some("token".into()),
            identity_token: None,
            allow_unauthenticated_invoke: false,
            function_service_account: None,
            build_service_account: None,
            gateway_domain: "cpu.example.invalid".into(),
            payment_policy_database: None,
        })
        .unwrap();

        assert_eq!(driver.cleanup_channel("verifiedchannel").await.unwrap(), 1);
        server.abort();
    }

    #[test]
    fn channel_ids_map_to_stable_label_safe_lease_keys() {
        assert_eq!(
            channel_lease_key("verifiedchannel").unwrap(),
            "8e8a355d709e16245dcd6748262bec1a"
        );
        assert!(channel_lease_key("").is_err());
        assert!(
            require_channel_lease(
                "verifiedchannel",
                &json!({ "labels": { "pay-channel": "other" } })
            )
            .is_err()
        );
    }

    #[test]
    fn inline_source_is_zipped_and_rejects_traversal() {
        let source = SourceInput::Inline {
            files: BTreeMap::from([("index.js".into(), "exports.hello = () => 'ok'".into())]),
        };
        assert!(source_zip(&source).unwrap().starts_with(b"PK\x03\x04"));
        let bad = SourceInput::Inline {
            files: BTreeMap::from([("../secret".into(), "no".into())]),
        };
        assert!(source_zip(&bad).is_err());
    }

    #[test]
    fn function_names_are_strict() {
        assert!(validate_function_name("hello-world").is_ok());
        assert!(validate_function_name("Bad_Name").is_err());
        assert!(validate_function_name("abc").is_err());
    }

    #[test]
    fn invocation_path_cannot_replace_origin() {
        let mut url = url::Url::parse("https://service.run.app").unwrap();
        assert!(apply_relative_path(&mut url, "//attacker.example/x").is_err());
        apply_relative_path(&mut url, "/v1?q=yes").unwrap();
        assert_eq!(url.as_str(), "https://service.run.app/v1?q=yes");
    }

    #[test]
    fn public_paths_are_explicit_and_cannot_publish_traversal() {
        assert_eq!(
            validate_public_paths(&["/summary".into(), "/summary".into()]).unwrap(),
            vec!["/summary"]
        );
        assert!(validate_public_paths(&[]).is_err());
        assert!(validate_public_paths(&["/../refresh".into()]).is_err());
        assert!(validate_public_paths(&["/summary?admin=true".into()]).is_err());
        assert!(path_is_public(&["/summary".into()], "/summary?limit=5"));
        assert!(!path_is_public(&["/summary".into()], "/refresh"));
        for path in [
            "/a/%2e%2e/refresh",
            "/a/./refresh",
            "/a\\refresh",
            "/internal/run",
            "/__402/payment-policy",
        ] {
            assert!(validate_public_paths(&[path.into()]).is_err());
            assert!(!path_is_public(&[path.into()], path));
        }
    }
}
