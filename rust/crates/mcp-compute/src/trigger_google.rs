use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::trigger_driver::{Result, TriggerDriver, TriggerError, TriggerExecutionResponse};
use crate::trigger_types::{
    CreateTriggerRequest, ExecuteTriggerRequest, ListTriggersRequest, Trigger,
    TriggerConfiguration, TriggerDriverCapabilities, TriggerList, TriggerRequest,
};
use crate::types::Tenant;

pub const DRIVER_ID: &str = "google-cloud-scheduler";
const SCHEDULER_API: &str = "https://cloudscheduler.googleapis.com";
const FUNCTIONS_API: &str = "https://cloudfunctions.googleapis.com";
const METADATA_API: &str = "http://metadata.google.internal/computeMetadata/v1";
const PREPAID_RUNS: u32 = 100;
const BUDGET_LIFETIME_SECONDS: u64 = 7 * 24 * 60 * 60;
const MAX_BUDGETED_TIMEOUT_SECONDS: u64 = 60;
const MAX_BUDGETED_MEMORY_MIB: f64 = 512.0;
const MAX_BUDGETED_CPU: f64 = 1.0;
const MAX_EXECUTION_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct GoogleTriggerConfig {
    pub project: String,
    pub default_region: String,
    pub scheduler_api_base: String,
    pub functions_api_base: String,
    pub metadata_base: String,
    pub access_token: Option<String>,
    pub redis_url: String,
    pub executor_url: String,
    pub executor_proof: String,
}

impl GoogleTriggerConfig {
    pub fn from_env() -> Result<Self> {
        let value = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let project = value("COMPUTE_TRIGGER_GOOGLE_PROJECT")
            .or_else(|| value("GOOGLE_CLOUD_PROJECT"))
            .ok_or_else(|| {
                TriggerError::Configuration(
                    "COMPUTE_TRIGGER_GOOGLE_PROJECT or GOOGLE_CLOUD_PROJECT must be set".into(),
                )
            })?;
        Ok(Self {
            project,
            default_region: value("COMPUTE_TRIGGER_GOOGLE_REGION")
                .unwrap_or_else(|| "us-central1".into()),
            scheduler_api_base: value("COMPUTE_TRIGGER_GOOGLE_SCHEDULER_API_BASE")
                .unwrap_or_else(|| SCHEDULER_API.into())
                .trim_end_matches('/')
                .into(),
            functions_api_base: value("COMPUTE_TRIGGER_GOOGLE_FUNCTIONS_API_BASE")
                .unwrap_or_else(|| FUNCTIONS_API.into())
                .trim_end_matches('/')
                .into(),
            metadata_base: value("COMPUTE_TRIGGER_GOOGLE_METADATA_BASE")
                .unwrap_or_else(|| METADATA_API.into())
                .trim_end_matches('/')
                .into(),
            access_token: value("COMPUTE_TRIGGER_GOOGLE_ACCESS_TOKEN")
                .or_else(|| value("GOOGLE_OAUTH_ACCESS_TOKEN")),
            redis_url: value("COMPUTE_TRIGGER_REDIS_URL")
                .or_else(|| value("PAY_SESSION_REDIS_URL"))
                .ok_or_else(|| {
                    TriggerError::Configuration(
                        "COMPUTE_TRIGGER_REDIS_URL or PAY_SESSION_REDIS_URL must be set".into(),
                    )
                })?,
            executor_url: value("COMPUTE_TRIGGER_EXECUTOR_URL")
                .ok_or_else(|| {
                    TriggerError::Configuration("COMPUTE_TRIGGER_EXECUTOR_URL must be set".into())
                })?
                .trim_end_matches('/')
                .into(),
            executor_proof: value("COMPUTE_TRIGGER_EXECUTOR_PROOF").ok_or_else(|| {
                TriggerError::Configuration("COMPUTE_TRIGGER_EXECUTOR_PROOF must be set".into())
            })?,
        })
    }
}

#[derive(Clone)]
pub struct GoogleTriggerDriver {
    config: Arc<GoogleTriggerConfig>,
    client: reqwest::Client,
    token: Arc<RwLock<Option<(String, Instant)>>>,
    budgets: redis::aio::ConnectionManager,
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
    expires_in: u64,
}

impl GoogleTriggerDriver {
    pub async fn new(config: GoogleTriggerConfig) -> Result<Self> {
        validate_segment("project", &config.project)?;
        validate_segment("region", &config.default_region)?;
        if config.executor_proof.len() < 32 {
            return Err(TriggerError::Configuration(
                "COMPUTE_TRIGGER_EXECUTOR_PROOF must contain at least 32 bytes".into(),
            ));
        }
        let redis = redis::Client::open(config.redis_url.clone())?;
        let budgets = redis.get_connection_manager().await?;
        Ok(Self {
            config: Arc::new(config),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            token: Arc::new(RwLock::new(None)),
            budgets,
        })
    }

    async fn identity_token(&self, audience: &str) -> Result<String> {
        let response = self
            .client
            .get(format!(
                "{}/instance/service-accounts/default/identity",
                self.config.metadata_base
            ))
            .query(&[("audience", audience), ("format", "full")])
            .header("Metadata-Flavor", "Google")
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(provider_error(status, &bytes));
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| {
            TriggerError::Provider("metadata returned a non-UTF-8 identity token".into())
        })
    }

    fn budget_key(resource: &str) -> String {
        format!(
            "pay:compute:trigger-budget:{:x}",
            Sha256::digest(resource.as_bytes())
        )
    }

    async fn fund_budget(&self, resource: &str, channel: &str) -> Result<()> {
        let expires_at = unix_seconds()?.saturating_add(BUDGET_LIFETIME_SECONDS);
        let mut connection = self.budgets.clone();
        redis::pipe()
            .atomic()
            .hset(Self::budget_key(resource), "channel", channel)
            .hset(Self::budget_key(resource), "remaining", PREPAID_RUNS)
            .hset(Self::budget_key(resource), "expires_at", expires_at)
            .expire(
                Self::budget_key(resource),
                i64::try_from(BUDGET_LIFETIME_SECONDS + 3600).unwrap_or(i64::MAX),
            )
            .query_async::<()>(&mut connection)
            .await?;
        Ok(())
    }

    async fn consume_budget(&self, resource: &str, channel: &str) -> Result<bool> {
        let script = redis::Script::new(
            r#"
local stored = redis.call('HGET', KEYS[1], 'channel')
if not stored or stored ~= ARGV[1] then return -3 end
local expires = tonumber(redis.call('HGET', KEYS[1], 'expires_at') or '0')
if expires <= tonumber(ARGV[2]) then return -2 end
local remaining = tonumber(redis.call('HGET', KEYS[1], 'remaining') or '0')
if remaining <= 0 then return -1 end
return redis.call('HINCRBY', KEYS[1], 'remaining', -1)
"#,
        );
        let mut connection = self.budgets.clone();
        let remaining: i64 = script
            .key(Self::budget_key(resource))
            .arg(channel)
            .arg(unix_seconds()?)
            .invoke_async(&mut connection)
            .await?;
        Ok(remaining >= 0)
    }

    async fn delete_budget(&self, resource: &str) -> Result<()> {
        let mut connection = self.budgets.clone();
        redis::cmd("DEL")
            .arg(Self::budget_key(resource))
            .query_async::<()>(&mut connection)
            .await?;
        Ok(())
    }

    async fn token(&self) -> Result<String> {
        if let Some(value) = &self.config.access_token {
            return Ok(value.clone());
        }
        if let Some((value, until)) = self.token.read().await.as_ref()
            && Instant::now() < *until
        {
            return Ok(value.clone());
        }
        let response = self
            .client
            .get(format!(
                "{}/instance/service-accounts/default/token",
                self.config.metadata_base
            ))
            .header("Metadata-Flavor", "Google")
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(provider_error(status, &bytes));
        }
        let token: MetadataToken = serde_json::from_slice(&bytes)?;
        let until =
            Instant::now() + Duration::from_secs(token.expires_in.saturating_sub(60).max(1));
        *self.token.write().await = Some((token.access_token.clone(), until));
        Ok(token.access_token)
    }

    async fn request(
        &self,
        method: Method,
        url: String,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(self.token().await?);
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

    async fn require(&self, method: Method, url: String, body: Option<&Value>) -> Result<Value> {
        let (status, value) = self.request(method, url, body).await?;
        if status.is_success() {
            Ok(value)
        } else {
            Err(TriggerError::Provider(format!(
                "Google API returned {status}: {value}"
            )))
        }
    }

    fn region<'a>(&'a self, requested: Option<&'a str>) -> Result<&'a str> {
        let region = requested.unwrap_or(&self.config.default_region);
        validate_segment("region", region)?;
        if region != self.config.default_region {
            return Err(TriggerError::InvalidRequest(format!(
                "this trigger driver is configured for region `{}`",
                self.config.default_region
            )));
        }
        Ok(region)
    }

    fn physical_name(&self, tenant: &Tenant, name: &str) -> Result<String> {
        validate_segment("trigger name", name)?;
        Ok(format!("trigger-{}-{name}", tenant.key))
    }

    fn resource(&self, region: &str, id: &str) -> Result<String> {
        let physical = id.rsplit('/').next().unwrap_or(id);
        validate_segment("trigger ID", physical)?;
        Ok(format!(
            "projects/{}/locations/{region}/jobs/{physical}",
            self.config.project
        ))
    }

    async fn owned_trigger(
        &self,
        tenant: &Tenant,
        request: &TriggerRequest,
    ) -> Result<(String, Value)> {
        let region = self.region(request.region.as_deref())?;
        let resource = self.resource(region, &request.id)?;
        let value = self
            .require(
                Method::GET,
                format!("{}/v1/{resource}", self.config.scheduler_api_base),
                None,
            )
            .await?;
        require_owner(&value, tenant)?;
        Ok((region.into(), value))
    }

    async fn cleanup_markers(&self, markers: &[String]) -> Result<usize> {
        let region = &self.config.default_region;
        let mut resources = Vec::new();
        let mut page_token = None;
        loop {
            let mut url = reqwest::Url::parse(&format!(
                "{}/v1/projects/{}/locations/{region}/jobs",
                self.config.scheduler_api_base, self.config.project
            ))
            .map_err(|error| TriggerError::Configuration(error.to_string()))?;
            url.query_pairs_mut().append_pair("pageSize", "500");
            if let Some(token) = page_token.as_deref() {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let value = self.require(Method::GET, url.into(), None).await?;
            for trigger in value
                .get("jobs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if trigger
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|description| {
                        markers.iter().all(|marker| description.contains(marker))
                    })
                    && let Some(name) = trigger.get("name").and_then(Value::as_str)
                {
                    resources.push(name.to_string());
                }
            }
            page_token = value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            if page_token.is_none() {
                break;
            }
        }

        let mut deleted = 0;
        for name in resources {
            let (status, current) = self
                .request(
                    Method::GET,
                    format!("{}/v1/{name}", self.config.scheduler_api_base),
                    None,
                )
                .await?;
            if status == StatusCode::NOT_FOUND {
                continue;
            }
            if !status.is_success() {
                return Err(TriggerError::Provider(format!(
                    "Google API returned {status}: {current}"
                )));
            }
            if !current
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|description| {
                    markers.iter().all(|marker| description.contains(marker))
                })
            {
                continue;
            }
            let (status, value) = self
                .request(
                    Method::DELETE,
                    format!("{}/v1/{name}", self.config.scheduler_api_base),
                    None,
                )
                .await?;
            if status.is_success() || status == StatusCode::NOT_FOUND {
                self.delete_budget(&name).await?;
                deleted += 1;
            } else {
                return Err(TriggerError::Provider(format!(
                    "Google API returned {status}: {value}"
                )));
            }
        }
        Ok(deleted)
    }
}

#[async_trait]
impl TriggerDriver for GoogleTriggerDriver {
    fn id(&self) -> &'static str {
        DRIVER_ID
    }

    fn capabilities(&self) -> TriggerDriverCapabilities {
        TriggerDriverCapabilities {
            driver: DRIVER_ID.into(),
            display_name: "Google Cloud Scheduler".into(),
            operations: vec![
                "create".into(),
                "get".into(),
                "list".into(),
                "pause".into(),
                "resume".into(),
                "run_now".into(),
                "delete".into(),
            ],
            target_providers: vec!["google-cloud-functions".into()],
            default_region: self.config.default_region.clone(),
        }
    }

    async fn create(&self, tenant: &Tenant, request: CreateTriggerRequest) -> Result<Trigger> {
        let (cron, timezone) = match &request.configuration {
            TriggerConfiguration::Schedule { cron, timezone } => (cron, timezone),
        };
        validate_schedule(cron, timezone, &request.target.path)?;
        if request.target.provider != "google-cloud-functions" {
            return Err(TriggerError::InvalidRequest(
                "Google schedule triggers require a google-cloud-functions target".into(),
            ));
        }
        let region = self.region(request.region.as_deref())?;
        let (target_region, target_name) = if request.target.id.contains('/') {
            parse_function_resource(
                &request.target.id,
                &self.config.project,
                request.target.region.as_deref(),
            )?
        } else {
            let target_region = request.target.region.as_deref().unwrap_or(region);
            validate_segment("target region", target_region)?;
            validate_segment("target ID", &request.target.id)?;
            let target_name = if request.target.id.starts_with("gcf-") {
                request.target.id.clone()
            } else {
                format!("gcf-{}-{}", tenant.key, request.target.id)
            };
            (target_region.to_string(), target_name)
        };
        let function_resource = format!(
            "projects/{}/locations/{target_region}/functions/{target_name}",
            self.config.project
        );
        let function = self
            .require(
                Method::GET,
                format!("{}/v2/{function_resource}", self.config.functions_api_base),
                None,
            )
            .await?;
        require_function_owner(&function, tenant)?;
        validate_budgeted_function(&function)?;
        let physical = self.physical_name(tenant, &request.name)?;
        let resource = self.resource(region, &physical)?;
        let channel = lease_key(&tenant.channel_id)?;
        let (existing_status, existing) = self
            .request(
                Method::GET,
                format!("{}/v1/{resource}", self.config.scheduler_api_base),
                None,
            )
            .await?;
        if existing_status.is_success() {
            require_owner(&existing, tenant)?;
            require_channel(&existing, &channel)?;
        } else if existing_status != StatusCode::NOT_FOUND {
            return Err(TriggerError::Provider(format!(
                "Google API returned {existing_status}: {existing}"
            )));
        }
        let mut headers = request
            .headers
            .into_iter()
            .filter(|(name, _)| {
                let name = name.to_ascii_lowercase();
                name != "authorization"
                    && name != "host"
                    && name != "content-length"
                    && !name.starts_with("payment-")
                    && !name.starts_with("x-payment")
                    && !name.starts_with("x-pay-")
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        headers.insert("content-type".into(), "application/json".into());
        let executor_body = json!({
            "driver": DRIVER_ID,
            "trigger_resource": resource,
            "channel_lease": channel,
            "target_resource": function_resource,
            "tenant_key": tenant.key,
            "path": request.target.path,
            "input": request.input,
            "headers": headers,
        });
        let body = json!({
            "name": resource,
            "description": format!("managed-by=mcp-compute;resource=trigger;pay-tenant={};pay-channel={channel};pay-name={};pay-target-region={target_region};pay-target={target_name};prepaid-runs={PREPAID_RUNS}", tenant.key, request.name),
            "schedule": cron,
            "timeZone": timezone,
            "retryConfig": { "retryCount": 0 },
            "httpTarget": {
                "uri": format!("{}/internal/triggers/run", self.config.executor_url),
                "httpMethod": "POST",
                "headers": {
                    "content-type": "application/json",
                    "x-compute-trigger-proof": self.config.executor_proof,
                },
                "body": base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&executor_body)?),
            }
        });
        let url = format!(
            "{}/v1/{resource}?updateMask=schedule,timeZone,retryConfig,httpTarget,description",
            self.config.scheduler_api_base
        );
        let (status, value) = self.request(Method::PATCH, url, Some(&body)).await?;
        let value = if status == StatusCode::NOT_FOUND {
            self.require(
                Method::POST,
                format!(
                    "{}/v1/projects/{}/locations/{region}/jobs",
                    self.config.scheduler_api_base, self.config.project
                ),
                Some(&body),
            )
            .await?
        } else if status.is_success() {
            value
        } else {
            return Err(TriggerError::Provider(format!(
                "Google API returned {status}: {value}"
            )));
        };
        if let Err(error) = self.fund_budget(&resource, &channel).await {
            let _ = self
                .request(
                    Method::POST,
                    format!("{}/v1/{resource}:pause", self.config.scheduler_api_base),
                    Some(&json!({})),
                )
                .await;
            return Err(error);
        }
        trigger_from_value(&value, region)
    }

    async fn get(&self, tenant: &Tenant, request: TriggerRequest) -> Result<Trigger> {
        let (region, value) = self.owned_trigger(tenant, &request).await?;
        trigger_from_value(&value, &region)
    }

    async fn list(&self, tenant: &Tenant, request: ListTriggersRequest) -> Result<TriggerList> {
        let region = self.region(request.region.as_deref())?;
        let mut url = reqwest::Url::parse(&format!(
            "{}/v1/projects/{}/locations/{region}/jobs",
            self.config.scheduler_api_base, self.config.project
        ))
        .map_err(|e| TriggerError::Configuration(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("pageSize", &request.page_size.clamp(1, 100).to_string());
        if let Some(token) = request.page_token {
            url.query_pairs_mut().append_pair("pageToken", &token);
        }
        let value = self.require(Method::GET, url.into(), None).await?;
        let triggers = value
            .get("jobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|value| require_owner(value, tenant).is_ok())
            .filter_map(|value| trigger_from_value(value, region).ok())
            .collect();
        Ok(TriggerList {
            triggers,
            next_page_token: value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(Into::into),
        })
    }

    async fn pause(&self, tenant: &Tenant, request: TriggerRequest) -> Result<Trigger> {
        action(self, tenant, request, "pause").await
    }
    async fn resume(&self, tenant: &Tenant, request: TriggerRequest) -> Result<Trigger> {
        action(self, tenant, request, "resume").await
    }
    async fn run_now(&self, tenant: &Tenant, request: TriggerRequest) -> Result<Trigger> {
        action(self, tenant, request, "run").await
    }

    async fn delete(&self, tenant: &Tenant, request: TriggerRequest) -> Result<()> {
        let (region, _) = self.owned_trigger(tenant, &request).await?;
        let resource = self.resource(&region, &request.id)?;
        self.require(
            Method::DELETE,
            format!("{}/v1/{resource}", self.config.scheduler_api_base),
            None,
        )
        .await?;
        self.delete_budget(&resource).await?;
        Ok(())
    }

    async fn cleanup_target(
        &self,
        tenant: &Tenant,
        provider: &str,
        id: &str,
        region: Option<&str>,
    ) -> Result<usize> {
        if provider != "google-cloud-functions" {
            return Ok(0);
        }
        let (target_region, target_name) = if id.contains('/') {
            parse_function_resource(id, &self.config.project, region)?
        } else if id.starts_with("gcf-") {
            let target_region = region.unwrap_or(&self.config.default_region);
            validate_segment("target region", target_region)?;
            validate_segment("target ID", id)?;
            (target_region.to_string(), id.to_string())
        } else {
            let target_region = region.unwrap_or(&self.config.default_region);
            validate_segment("target region", target_region)?;
            validate_segment("target ID", id)?;
            (
                target_region.to_string(),
                format!("gcf-{}-{id}", tenant.key),
            )
        };
        self.cleanup_markers(&[
            format!("pay-tenant={};", tenant.key),
            format!("pay-target-region={target_region};"),
            format!("pay-target={target_name};"),
        ])
        .await
    }

    async fn cleanup_channel(&self, channel_id: &str) -> Result<usize> {
        self.cleanup_markers(&[format!("pay-channel={};", lease_key(channel_id)?)])
            .await
    }

    async fn execute(&self, request: ExecuteTriggerRequest) -> Result<TriggerExecutionResponse> {
        let region = self.region(None)?;
        let expected_prefix = format!(
            "projects/{}/locations/{region}/jobs/trigger-",
            self.config.project
        );
        if !request.trigger_resource.starts_with(&expected_prefix) {
            return Err(TriggerError::InvalidRequest(
                "executor trigger resource is outside the configured project and region".into(),
            ));
        }
        if !self
            .consume_budget(&request.trigger_resource, &request.channel_lease)
            .await?
        {
            let _ = self
                .request(
                    Method::POST,
                    format!(
                        "{}/v1/{}:pause",
                        self.config.scheduler_api_base, request.trigger_resource
                    ),
                    Some(&json!({})),
                )
                .await;
            return Err(TriggerError::InvalidRequest(
                "prepaid execution budget is exhausted or expired".into(),
            ));
        }
        validate_schedule("* * * * *", "Etc/UTC", &request.path)?;
        let (target_region, target_name) =
            parse_function_resource(&request.target_resource, &self.config.project, None)?;
        let target_resource = format!(
            "projects/{}/locations/{target_region}/functions/{target_name}",
            self.config.project
        );
        if target_resource != request.target_resource {
            return Err(TriggerError::InvalidRequest(
                "executor target resource is not canonical".into(),
            ));
        }
        let function = self
            .require(
                Method::GET,
                format!("{}/v2/{target_resource}", self.config.functions_api_base),
                None,
            )
            .await?;
        require_function_tenant(&function, &request.tenant_key)?;
        validate_budgeted_function(&function)?;
        let origin = function
            .pointer("/serviceConfig/uri")
            .and_then(Value::as_str)
            .ok_or_else(|| TriggerError::Provider("target function has no service URI".into()))?;
        let url = target_url(origin, &request.path)?;
        let token = self.identity_token(origin).await?;
        let mut outgoing = self.client.post(url).bearer_auth(token);
        for (name, value) in request.headers {
            outgoing = outgoing.header(name, value);
        }
        let response = outgoing.json(&request.input).send().await?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                matches!(name.as_str(), "content-type" | "etag" | "cache-control")
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
            .is_some_and(|length| length > MAX_EXECUTION_RESPONSE_BYTES)
        {
            return Err(TriggerError::Provider(
                "scheduled function response exceeds 1 MiB".into(),
            ));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len().saturating_add(chunk.len()) > MAX_EXECUTION_RESPONSE_BYTES as usize {
                return Err(TriggerError::Provider(
                    "scheduled function response exceeds 1 MiB".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(TriggerExecutionResponse {
            status,
            headers,
            body: body.into(),
        })
    }
}

async fn action(
    driver: &GoogleTriggerDriver,
    tenant: &Tenant,
    request: TriggerRequest,
    verb: &str,
) -> Result<Trigger> {
    let (region, _) = driver.owned_trigger(tenant, &request).await?;
    let resource = driver.resource(&region, &request.id)?;
    let value = driver
        .require(
            Method::POST,
            format!("{}/v1/{resource}:{verb}", driver.config.scheduler_api_base),
            Some(&json!({})),
        )
        .await?;
    trigger_from_value(&value, &region)
}

fn require_owner(value: &Value, tenant: &Tenant) -> Result<()> {
    let marker = format!("pay-tenant={}", tenant.key);
    if value
        .get("description")
        .and_then(Value::as_str)
        .is_some_and(|d| d.contains(&marker))
    {
        Ok(())
    } else {
        Err(TriggerError::InvalidRequest(
            "trigger is not owned by the verified payer".into(),
        ))
    }
}

fn require_channel(value: &Value, expected: &str) -> Result<()> {
    let marker = format!("pay-channel={expected}");
    if value
        .get("description")
        .and_then(Value::as_str)
        .is_some_and(|description| description.contains(&marker))
    {
        Ok(())
    } else {
        Err(TriggerError::InvalidRequest(
            "an existing trigger cannot move between funding channels".into(),
        ))
    }
}

fn parse_function_resource(
    resource: &str,
    expected_project: &str,
    requested_region: Option<&str>,
) -> Result<(String, String)> {
    let parts: Vec<_> = resource.split('/').collect();
    if parts.len() != 6
        || parts[0] != "projects"
        || parts[2] != "locations"
        || parts[4] != "functions"
        || parts[1] != expected_project
    {
        return Err(TriggerError::InvalidRequest(
            "target ID must be a function in the configured Google project".into(),
        ));
    }
    validate_segment("target region", parts[3])?;
    validate_segment("target ID", parts[5])?;
    if requested_region.is_some_and(|region| region != parts[3]) {
        return Err(TriggerError::InvalidRequest(
            "target.region conflicts with the region in target.id".into(),
        ));
    }
    Ok((parts[3].to_string(), parts[5].to_string()))
}

fn require_function_owner(value: &Value, tenant: &Tenant) -> Result<()> {
    require_function_tenant(value, &tenant.key)
}

fn require_function_tenant(value: &Value, tenant_key: &str) -> Result<()> {
    validate_segment("tenant key", tenant_key)?;
    if value.pointer("/labels/managed-by").and_then(Value::as_str) == Some("mcp-compute")
        && value.pointer("/labels/pay-tenant").and_then(Value::as_str) == Some(tenant_key)
    {
        Ok(())
    } else {
        Err(TriggerError::InvalidRequest(
            "target function is not owned by the verified payer".into(),
        ))
    }
}

fn validate_budgeted_function(value: &Value) -> Result<()> {
    let timeout = value
        .pointer("/serviceConfig/timeoutSeconds")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str()?.trim_end_matches('s').parse().ok())
        })
        .unwrap_or(300);
    let memory_mib = value
        .pointer("/serviceConfig/availableMemory")
        .and_then(Value::as_str)
        .and_then(parse_memory_mib)
        .unwrap_or(256.0);
    let cpu = value
        .pointer("/serviceConfig/availableCpu")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(1.0);
    if timeout > MAX_BUDGETED_TIMEOUT_SECONDS
        || memory_mib > MAX_BUDGETED_MEMORY_MIB
        || cpu > MAX_BUDGETED_CPU
    {
        return Err(TriggerError::InvalidRequest(format!(
            "prepaid triggers require target limits of at most {MAX_BUDGETED_TIMEOUT_SECONDS}s, \
             {MAX_BUDGETED_MEMORY_MIB:.0} MiB, and {MAX_BUDGETED_CPU:.0} vCPU"
        )));
    }
    Ok(())
}

fn parse_memory_mib(value: &str) -> Option<f64> {
    if let Some(mib) = value.strip_suffix('M') {
        return mib.parse().ok();
    }
    if let Some(gib) = value.strip_suffix("Gi") {
        return gib.parse::<f64>().ok().map(|amount| amount * 1024.0);
    }
    if let Some(gb) = value.strip_suffix('G') {
        return gb.parse::<f64>().ok().map(|amount| amount * 1024.0);
    }
    value.parse().ok()
}

fn trigger_from_value(value: &Value, region: &str) -> Result<Trigger> {
    let id = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| TriggerError::Provider("trigger response has no name".into()))?;
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = description
        .split(';')
        .find_map(|p| p.strip_prefix("pay-name="))
        .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id));
    Ok(Trigger {
        driver: DRIVER_ID.into(),
        id: id.into(),
        name: name.into(),
        region: region.into(),
        configuration: TriggerConfiguration::Schedule {
            cron: value
                .get("schedule")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            timezone: value
                .get("timeZone")
                .and_then(Value::as_str)
                .unwrap_or("Etc/UTC")
                .into(),
        },
        state: value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("STATE_UNSPECIFIED")
            .into(),
        target: value
            .pointer("/httpTarget/uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        updated_at: value
            .get("userUpdateTime")
            .and_then(Value::as_str)
            .map(Into::into),
    })
}

fn validate_schedule(cron: &str, timezone: &str, path: &str) -> Result<()> {
    if cron.len() > 128 || cron.split_ascii_whitespace().count() != 5 {
        return Err(TriggerError::InvalidRequest(
            "cron must contain five fields".into(),
        ));
    }
    if timezone.is_empty() || timezone.len() > 128 || timezone.contains(['\r', '\n']) {
        return Err(TriggerError::InvalidRequest("timezone is invalid".into()));
    }
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains(['\r', '\n', '#'])
    {
        return Err(TriggerError::InvalidRequest(
            "target path is invalid".into(),
        ));
    }
    Ok(())
}

fn target_url(origin: &str, target_path: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(origin)
        .map_err(|error| TriggerError::InvalidRequest(format!("invalid target origin: {error}")))?;
    if url.scheme() != "https" {
        return Err(TriggerError::InvalidRequest(
            "target origin must use HTTPS".into(),
        ));
    }
    let (path, query) = target_path
        .split_once('?')
        .map_or((target_path, None), |(path, query)| (path, Some(query)));
    url.set_path(&format!("{}{}", url.path().trim_end_matches('/'), path));
    url.set_query(query);
    Ok(url)
}

fn validate_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(TriggerError::InvalidRequest(format!(
            "{label} must use lowercase letters, digits, and hyphens"
        )));
    }
    Ok(())
}

fn lease_key(channel_id: &str) -> Result<String> {
    if channel_id.is_empty()
        || channel_id.len() > 128
        || !channel_id.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(TriggerError::InvalidRequest(
            "verified payment channel is invalid".into(),
        ));
    }
    Ok(Sha256::digest(channel_id.as_bytes())[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn unix_seconds() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| TriggerError::Configuration("system clock is before the Unix epoch".into()))
}

fn provider_error(status: StatusCode, bytes: &[u8]) -> TriggerError {
    TriggerError::Provider(format!(
        "Google metadata returned {status}: {}",
        String::from_utf8_lossy(bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_paths_are_private_origin_relative() {
        assert!(validate_schedule("*/5 * * * *", "America/New_York", "/refresh").is_ok());
        assert!(validate_schedule("* * * *", "Etc/UTC", "/").is_err());
        assert!(validate_schedule("*/5 * * * *", "Etc/UTC", "//evil").is_err());
        assert!(validate_schedule("*/5 * * * *", "Etc/UTC", "/refresh#fragment").is_err());
    }

    #[test]
    fn target_urls_preserve_query_parameters() {
        let url = target_url(
            "https://function.example.invalid",
            "/refresh?units=metric&limit=1",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://function.example.invalid/refresh?units=metric&limit=1"
        );
    }

    #[test]
    fn full_function_resource_preserves_region() {
        assert_eq!(
            parse_function_resource(
                "projects/project/locations/europe-west1/functions/gcf-tenant-weather",
                "project",
                None,
            )
            .unwrap(),
            ("europe-west1".into(), "gcf-tenant-weather".into())
        );
        assert!(
            parse_function_resource(
                "projects/project/locations/europe-west1/functions/gcf-tenant-weather",
                "project",
                Some("us-central1"),
            )
            .is_err()
        );
        assert!(
            parse_function_resource(
                "projects/other/locations/europe-west1/functions/gcf-tenant-weather",
                "project",
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn target_cleanup_markers_include_region() {
        let description = "managed-by=mcp-compute;resource=trigger;pay-tenant=tenant;pay-target-region=europe-west1;pay-target=gcf-tenant-weather;";
        let europe_markers = [
            "pay-tenant=tenant;",
            "pay-target-region=europe-west1;",
            "pay-target=gcf-tenant-weather;",
        ];
        let us_markers = [
            "pay-tenant=tenant;",
            "pay-target-region=us-central1;",
            "pay-target=gcf-tenant-weather;",
        ];
        assert!(
            europe_markers
                .iter()
                .all(|marker| description.contains(marker))
        );
        assert!(!us_markers.iter().all(|marker| description.contains(marker)));
    }

    #[test]
    fn trigger_updates_cannot_move_funding_channels() {
        let value = json!({
            "description": "managed-by=mcp-compute;resource=trigger;pay-channel=old;pay-name=weather"
        });
        assert!(require_channel(&value, "old").is_ok());
        assert!(require_channel(&value, "new").is_err());
    }

    #[test]
    fn prepaid_triggers_reject_unbounded_function_cost() {
        let bounded = json!({
            "serviceConfig": {
                "timeoutSeconds": 60,
                "availableMemory": "512M",
                "availableCpu": "1"
            }
        });
        assert!(validate_budgeted_function(&bounded).is_ok());
        for unbounded in [
            json!({"serviceConfig":{"timeoutSeconds":61,"availableMemory":"512M","availableCpu":"1"}}),
            json!({"serviceConfig":{"timeoutSeconds":60,"availableMemory":"1G","availableCpu":"1"}}),
            json!({"serviceConfig":{"timeoutSeconds":60,"availableMemory":"512M","availableCpu":"2"}}),
        ] {
            assert!(validate_budgeted_function(&unbounded).is_err());
        }
    }
}
