use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use zip::write::SimpleFileOptions;

use crate::driver::{ComputeDriver, ComputeError, Result};
use crate::types::{
    ComputeOperation, DeployRequest, Deployment, DeploymentList, DriverCapabilities, Exposure,
    GatewayInvocation, GatewayInvokeRequest, InvocationResult, InvokeRequest, ListRequest,
    OperationState, ResourceRequest, ResourceState, SourceInput, Tenant,
};

pub const DRIVER_ID: &str = "google-cloud-functions";
pub const GATEWAY_PREFIX: &str = "gcf-";
const DEFAULT_API_BASE: &str = "https://cloudfunctions.googleapis.com";
const DEFAULT_METADATA_BASE: &str = "http://metadata.google.internal/computeMetadata/v1";
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
    pub fn new(config: GoogleConfig) -> Result<Self> {
        validate_segment("project", &config.project)?;
        validate_segment("default region", &config.default_region)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            config: Arc::new(config),
            client,
            token: Arc::new(RwLock::new(None)),
        })
    }

    async fn access_token(&self) -> Result<String> {
        if let Some(token) = &self.config.access_token {
            return Ok(token.clone());
        }
        if let Some(cached) = self.token.read().await.as_ref() {
            if Instant::now() < cached.refresh_after {
                return Ok(cached.value.clone());
            }
        }
        let url = format!(
            "{}/instance/service-accounts/default/token",
            self.config.metadata_base
        );
        let response = self
            .client
            .get(url)
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

    async fn identity_token(&self, audience: &str) -> Result<Option<String>> {
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
        if request.limits.min_instances.unwrap_or(0) != 0 {
            return Err(ComputeError::InvalidRequest(
                "min_instances is not supported yet because idle instance cost cannot be attributed to an invocation; use 0 or omit it".into(),
            ));
        }
        let mut service = serde_json::Map::new();
        service.insert("timeoutSeconds".into(), json!(timeout));
        service.insert("ingressSettings".into(), json!(ingress));
        service.insert("environmentVariables".into(), json!(request.environment));
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
        Ok(json!({
            "name": resource_name,
            "description": options.description.unwrap_or_else(|| "Managed by Pay compute MCP".into()),
            "environment": "GEN_2",
            "labels": labels,
            "buildConfig": {
                "runtime": request.runtime.runtime,
                "entryPoint": request.runtime.entrypoint,
                "source": { "storageSource": storage_source }
            },
            "serviceConfig": Value::Object(service)
        }))
    }

    async fn function_exists(&self, resource_name: &str) -> Result<bool> {
        let (status, value) = self
            .send_json(
                Method::GET,
                format!("{}/v2/{resource_name}", self.config.api_base),
                None,
            )
            .await?;
        match status {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(json_provider_error(status, &value)),
        }
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
                "authorization" | "host" | "cookie" | "content-length" | "x-pay-compute-microusd"
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
        let body = response.bytes().await?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(ComputeError::Provider(format!(
                "function response exceeded {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        let elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        let billed_micro_usd = invocation_price_micro_usd(deployment, elapsed_ms, body.len());
        Ok(GatewayInvocation {
            status,
            headers,
            body,
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
            operations: vec!["deploy".into(), "get".into(), "list".into(), "invoke".into(), "delete".into(), "operation_status".into()],
            max_timeout_seconds: MAX_GATEWAY_EXECUTION_SECONDS,
            default_region: self.config.default_region.clone(),
            notes: vec![
                "Deployments are second-generation HTTP functions.".into(),
                "Provider origins stay private; invocation is performed by this MCP.".into(),
                "MPP authorizes and settles access, while timeout_seconds controls execution lifetime.".into(),
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
        let storage = self.source_storage(&request, &region).await?;
        let function = self.function_body(&request, &resource_name, storage, options)?;
        let exists = self.function_exists(&resource_name).await?;
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
        let value = self
            .require_json(
                Method::GET,
                format!("{}/v2/{resource}", self.config.api_base),
                None,
            )
            .await?;
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
        let resource = self.resource_name(tenant, &request.id, request.region.as_deref())?;
        let existing = self
            .require_json(
                Method::GET,
                format!("{}/v2/{resource}", self.config.api_base),
                None,
            )
            .await?;
        require_tenant_label(tenant, &existing)?;
        let value = self
            .require_json(
                Method::DELETE,
                format!("{}/v2/{resource}", self.config.api_base),
                None,
            )
            .await?;
        normalize_operation(value)
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
mod tests {
    use super::*;

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
}
