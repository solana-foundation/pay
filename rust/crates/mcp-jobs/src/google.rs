use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::driver::{JobDriver, JobError, Result};
use crate::types::{
    CreateJobRequest, DriverCapabilities, Job, JobList, JobRequest, ListJobsRequest, Tenant,
};

pub const DRIVER_ID: &str = "google-cloud-scheduler";
const SCHEDULER_API: &str = "https://cloudscheduler.googleapis.com";
const FUNCTIONS_API: &str = "https://cloudfunctions.googleapis.com";
const METADATA_API: &str = "http://metadata.google.internal/computeMetadata/v1";

#[derive(Clone, Debug)]
pub struct GoogleJobsConfig {
    pub project: String,
    pub default_region: String,
    pub scheduler_api_base: String,
    pub functions_api_base: String,
    pub metadata_base: String,
    pub access_token: Option<String>,
    pub scheduler_service_account: String,
}

impl GoogleJobsConfig {
    pub fn from_env() -> Result<Self> {
        let value = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let project = value("JOBS_GOOGLE_PROJECT")
            .or_else(|| value("GOOGLE_CLOUD_PROJECT"))
            .ok_or_else(|| {
                JobError::Configuration(
                    "JOBS_GOOGLE_PROJECT or GOOGLE_CLOUD_PROJECT must be set".into(),
                )
            })?;
        let scheduler_service_account =
            value("JOBS_GOOGLE_SCHEDULER_SERVICE_ACCOUNT").ok_or_else(|| {
                JobError::Configuration("JOBS_GOOGLE_SCHEDULER_SERVICE_ACCOUNT must be set".into())
            })?;
        Ok(Self {
            project,
            default_region: value("JOBS_GOOGLE_REGION").unwrap_or_else(|| "us-central1".into()),
            scheduler_api_base: value("JOBS_GOOGLE_SCHEDULER_API_BASE")
                .unwrap_or_else(|| SCHEDULER_API.into())
                .trim_end_matches('/')
                .into(),
            functions_api_base: value("JOBS_GOOGLE_FUNCTIONS_API_BASE")
                .unwrap_or_else(|| FUNCTIONS_API.into())
                .trim_end_matches('/')
                .into(),
            metadata_base: value("JOBS_GOOGLE_METADATA_BASE")
                .unwrap_or_else(|| METADATA_API.into())
                .trim_end_matches('/')
                .into(),
            access_token: value("JOBS_GOOGLE_ACCESS_TOKEN")
                .or_else(|| value("GOOGLE_OAUTH_ACCESS_TOKEN")),
            scheduler_service_account,
        })
    }
}

#[derive(Clone)]
pub struct GoogleJobsDriver {
    config: Arc<GoogleJobsConfig>,
    client: reqwest::Client,
    token: Arc<RwLock<Option<(String, Instant)>>>,
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
    expires_in: u64,
}

impl GoogleJobsDriver {
    pub fn new(config: GoogleJobsConfig) -> Result<Self> {
        validate_segment("project", &config.project)?;
        validate_segment("region", &config.default_region)?;
        Ok(Self {
            config: Arc::new(config),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            token: Arc::new(RwLock::new(None)),
        })
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
            Err(JobError::Provider(format!(
                "Google API returned {status}: {value}"
            )))
        }
    }

    fn region<'a>(&'a self, requested: Option<&'a str>) -> Result<&'a str> {
        let region = requested.unwrap_or(&self.config.default_region);
        validate_segment("region", region)?;
        if region != self.config.default_region {
            return Err(JobError::InvalidRequest(format!(
                "this jobs driver is configured for region `{}`",
                self.config.default_region
            )));
        }
        Ok(region)
    }

    fn physical_name(&self, tenant: &Tenant, name: &str) -> Result<String> {
        validate_segment("job name", name)?;
        Ok(format!("job-{}-{name}", tenant.key))
    }

    fn resource(&self, region: &str, id: &str) -> Result<String> {
        let physical = id.rsplit('/').next().unwrap_or(id);
        validate_segment("job ID", physical)?;
        Ok(format!(
            "projects/{}/locations/{region}/jobs/{physical}",
            self.config.project
        ))
    }

    async fn owned_job(&self, tenant: &Tenant, request: &JobRequest) -> Result<(String, Value)> {
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
}

#[async_trait]
impl JobDriver for GoogleJobsDriver {
    fn id(&self) -> &'static str {
        DRIVER_ID
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
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

    async fn create(&self, tenant: &Tenant, request: CreateJobRequest) -> Result<Job> {
        validate_schedule(&request.cron, &request.timezone, &request.target.path)?;
        if request.target.provider != "google-cloud-functions" {
            return Err(JobError::InvalidRequest(
                "Google Cloud Scheduler requires a google-cloud-functions target".into(),
            ));
        }
        let region = self.region(request.region.as_deref())?;
        let target_region = request.target.region.as_deref().unwrap_or(region);
        validate_segment("target region", target_region)?;
        let requested_target = request
            .target
            .id
            .rsplit('/')
            .next()
            .unwrap_or(&request.target.id);
        validate_segment("target ID", requested_target)?;
        let target_name = if request.target.id.contains('/') || requested_target.starts_with("gcf-")
        {
            requested_target.to_string()
        } else {
            format!("gcf-{}-{requested_target}", tenant.key)
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
        let origin = function
            .pointer("/serviceConfig/uri")
            .and_then(Value::as_str)
            .ok_or_else(|| JobError::Provider("target function has no service URI".into()))?;
        let physical = self.physical_name(tenant, &request.name)?;
        let resource = self.resource(region, &physical)?;
        let channel = lease_key(&tenant.channel_id)?;
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
        let body = json!({
            "name": resource,
            "description": format!("managed-by=mcp-jobs;pay-tenant={};pay-channel={channel};pay-name={}", tenant.key, request.name),
            "schedule": request.cron,
            "timeZone": request.timezone,
            "httpTarget": {
                "uri": format!("{}{}", origin.trim_end_matches('/'), request.target.path),
                "httpMethod": "POST",
                "headers": headers,
                "body": base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&request.input)?),
                "oidcToken": { "serviceAccountEmail": self.config.scheduler_service_account, "audience": origin }
            }
        });
        let url = format!(
            "{}/v1/{resource}?updateMask=schedule,timeZone,httpTarget,description",
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
            return Err(JobError::Provider(format!(
                "Google API returned {status}: {value}"
            )));
        };
        job_from_value(&value, region)
    }

    async fn get(&self, tenant: &Tenant, request: JobRequest) -> Result<Job> {
        let (region, value) = self.owned_job(tenant, &request).await?;
        job_from_value(&value, &region)
    }

    async fn list(&self, tenant: &Tenant, request: ListJobsRequest) -> Result<JobList> {
        let region = self.region(request.region.as_deref())?;
        let mut url = reqwest::Url::parse(&format!(
            "{}/v1/projects/{}/locations/{region}/jobs",
            self.config.scheduler_api_base, self.config.project
        ))
        .map_err(|e| JobError::Configuration(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("pageSize", &request.page_size.clamp(1, 100).to_string());
        if let Some(token) = request.page_token {
            url.query_pairs_mut().append_pair("pageToken", &token);
        }
        let value = self.require(Method::GET, url.into(), None).await?;
        let jobs = value
            .get("jobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|value| require_owner(value, tenant).is_ok())
            .filter_map(|value| job_from_value(value, region).ok())
            .collect();
        Ok(JobList {
            jobs,
            next_page_token: value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(Into::into),
        })
    }

    async fn pause(&self, tenant: &Tenant, request: JobRequest) -> Result<Job> {
        action(self, tenant, request, "pause").await
    }
    async fn resume(&self, tenant: &Tenant, request: JobRequest) -> Result<Job> {
        action(self, tenant, request, "resume").await
    }
    async fn run_now(&self, tenant: &Tenant, request: JobRequest) -> Result<Job> {
        action(self, tenant, request, "run").await
    }

    async fn delete(&self, tenant: &Tenant, request: JobRequest) -> Result<()> {
        let (region, _) = self.owned_job(tenant, &request).await?;
        let resource = self.resource(&region, &request.id)?;
        self.require(
            Method::DELETE,
            format!("{}/v1/{resource}", self.config.scheduler_api_base),
            None,
        )
        .await?;
        Ok(())
    }

    async fn cleanup_channel(&self, channel_id: &str) -> Result<usize> {
        let marker = format!("pay-channel={}", lease_key(channel_id)?);
        let region = &self.config.default_region;
        let mut resources = Vec::new();
        let mut page_token = None;
        loop {
            let mut url = reqwest::Url::parse(&format!(
                "{}/v1/projects/{}/locations/{region}/jobs",
                self.config.scheduler_api_base, self.config.project
            ))
            .map_err(|error| JobError::Configuration(error.to_string()))?;
            url.query_pairs_mut().append_pair("pageSize", "500");
            if let Some(token) = page_token.as_deref() {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let value = self.require(Method::GET, url.into(), None).await?;
            for job in value
                .get("jobs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if job
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|d| d.contains(&marker))
                    && let Some(name) = job.get("name").and_then(Value::as_str)
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
            let (status, value) = self
                .request(
                    Method::DELETE,
                    format!("{}/v1/{name}", self.config.scheduler_api_base),
                    None,
                )
                .await?;
            if status.is_success() || status == StatusCode::NOT_FOUND {
                deleted += 1;
            } else {
                return Err(JobError::Provider(format!(
                    "Google API returned {status}: {value}"
                )));
            }
        }
        Ok(deleted)
    }
}

async fn action(
    driver: &GoogleJobsDriver,
    tenant: &Tenant,
    request: JobRequest,
    verb: &str,
) -> Result<Job> {
    let (region, _) = driver.owned_job(tenant, &request).await?;
    let resource = driver.resource(&region, &request.id)?;
    let value = driver
        .require(
            Method::POST,
            format!("{}/v1/{resource}:{verb}", driver.config.scheduler_api_base),
            Some(&json!({})),
        )
        .await?;
    job_from_value(&value, &region)
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
        Err(JobError::InvalidRequest(
            "job is not owned by the verified payer".into(),
        ))
    }
}

fn require_function_owner(value: &Value, tenant: &Tenant) -> Result<()> {
    if value.pointer("/labels/managed-by").and_then(Value::as_str) == Some("mcp-compute")
        && value.pointer("/labels/pay-tenant").and_then(Value::as_str) == Some(&tenant.key)
    {
        Ok(())
    } else {
        Err(JobError::InvalidRequest(
            "target function is not owned by the verified payer".into(),
        ))
    }
}

fn job_from_value(value: &Value, region: &str) -> Result<Job> {
    let id = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| JobError::Provider("job response has no name".into()))?;
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = description
        .split(';')
        .find_map(|p| p.strip_prefix("pay-name="))
        .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id));
    Ok(Job {
        driver: DRIVER_ID.into(),
        id: id.into(),
        name: name.into(),
        region: region.into(),
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
        return Err(JobError::InvalidRequest(
            "cron must contain five fields".into(),
        ));
    }
    if timezone.is_empty() || timezone.len() > 128 || timezone.contains(['\r', '\n']) {
        return Err(JobError::InvalidRequest("timezone is invalid".into()));
    }
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains(['\r', '\n'])
    {
        return Err(JobError::InvalidRequest("target path is invalid".into()));
    }
    Ok(())
}

fn validate_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(JobError::InvalidRequest(format!(
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
        return Err(JobError::InvalidRequest(
            "verified payment channel is invalid".into(),
        ));
    }
    Ok(Sha256::digest(channel_id.as_bytes())[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn provider_error(status: StatusCode, bytes: &[u8]) -> JobError {
    JobError::Provider(format!(
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
    }
}
