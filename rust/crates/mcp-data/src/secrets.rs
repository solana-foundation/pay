use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::binding::{CreateBindingRequest, CreateBindingResponse, IssuedBinding};
use crate::driver::{DataError, Result};

const DEFAULT_API_BASE: &str = "https://secretmanager.googleapis.com";
const DEFAULT_METADATA_BASE: &str = "http://metadata.google.internal/computeMetadata/v1";

#[derive(Clone)]
pub struct BindingSecretStore {
    project: Arc<str>,
    api_base: Arc<str>,
    metadata_base: Arc<str>,
    access_token: Option<Arc<str>>,
    runtime_service_account: Arc<str>,
    client: reqwest::Client,
    token: Arc<RwLock<Option<CachedToken>>>,
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    refresh_after: Instant,
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
    expires_in: u64,
}

impl BindingSecretStore {
    pub fn from_env() -> Result<Self> {
        let project = env_required("DATA_GCP_PROJECT")?;
        let runtime_service_account = env_required("DATA_COMPUTE_RUNTIME_SERVICE_ACCOUNT")?;
        Ok(Self {
            project: project.into(),
            api_base: env_nonempty("DATA_SECRET_MANAGER_API_BASE")
                .unwrap_or_else(|| DEFAULT_API_BASE.into())
                .trim_end_matches('/')
                .to_string()
                .into(),
            metadata_base: env_nonempty("DATA_GCP_METADATA_BASE")
                .unwrap_or_else(|| DEFAULT_METADATA_BASE.into())
                .trim_end_matches('/')
                .to_string()
                .into(),
            access_token: env_nonempty("DATA_GCP_ACCESS_TOKEN")
                .or_else(|| env_nonempty("GOOGLE_OAUTH_ACCESS_TOKEN"))
                .map(Into::into),
            runtime_service_account: runtime_service_account.into(),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            token: Arc::new(RwLock::new(None)),
        })
    }

    pub async fn store(
        &self,
        tenant: &str,
        request: &CreateBindingRequest,
        issued: IssuedBinding,
    ) -> Result<CreateBindingResponse> {
        let secret_id = secret_id(tenant, request);
        let secret_url = format!(
            "{}/v1/projects/{}/secrets/{secret_id}",
            self.api_base, self.project
        );
        let (status, response) = self
            .send_json(Method::GET, secret_url.clone(), None)
            .await?;
        match status {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => {
                self.require_json(
                    Method::POST,
                    format!(
                        "{}/v1/projects/{}/secrets?secretId={secret_id}",
                        self.api_base, self.project
                    ),
                    Some(&json!({
                        "replication": { "automatic": {} },
                        "labels": {
                            "managed-by": "mcp-data",
                            "pay-tenant": tenant
                        }
                    })),
                )
                .await?;
            }
            _ => return Err(provider_error(status, &response)),
        }
        self.require_json(
            Method::POST,
            format!("{secret_url}:addVersion"),
            Some(&json!({
                "payload": {
                    "data": base64::engine::general_purpose::STANDARD
                        .encode(issued.capability.as_bytes())
                }
            })),
        )
        .await?;
        self.require_json(
            Method::POST,
            format!("{secret_url}:setIamPolicy"),
            Some(&json!({
                "policy": {
                    "bindings": [{
                        "role": "roles/secretmanager.secretAccessor",
                        "members": [format!("serviceAccount:{}", self.runtime_service_account)]
                    }]
                }
            })),
        )
        .await?;
        Ok(CreateBindingResponse {
            url_path: issued.url_path,
            secret_id,
            expires_at: issued.expires_at,
        })
    }

    async fn access_token(&self) -> Result<String> {
        if let Some(token) = &self.access_token {
            return Ok(token.to_string());
        }
        if let Some(cached) = self.token.read().await.as_ref()
            && Instant::now() < cached.refresh_after
        {
            return Ok(cached.value.clone());
        }
        let response = self
            .client
            .get(format!(
                "{}/instance/service-accounts/default/token",
                self.metadata_base
            ))
            .header("Metadata-Flavor", "Google")
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(DataError::Provider(format!(
                "metadata token endpoint returned {status}"
            )));
        }
        let token: MetadataToken = response.json().await?;
        let refresh_after =
            Instant::now() + Duration::from_secs(token.expires_in.saturating_sub(60).max(1));
        *self.token.write().await = Some(CachedToken {
            value: token.access_token.clone(),
            refresh_after,
        });
        Ok(token.access_token)
    }

    async fn send_json(
        &self,
        method: Method,
        url: String,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(self.access_token().await?);
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
            return Err(provider_error(status, &value));
        }
        Ok(value)
    }
}

fn secret_id(tenant: &str, request: &CreateBindingRequest) -> String {
    let digest = Sha256::digest(format!(
        "{}\0{}\0{}\0{}",
        tenant, request.workload_id, request.binding_name, request.store_id
    ));
    let suffix: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("pay-binding-{suffix}")
}

fn env_required(name: &str) -> Result<String> {
    env_nonempty(name).ok_or_else(|| DataError::Configuration(format!("{name} must be set")))
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn provider_error(status: StatusCode, _value: &Value) -> DataError {
    DataError::Provider(format!("Secret Manager returned {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_ids_are_stable_and_do_not_expose_resource_names() {
        let request = CreateBindingRequest {
            payer: "payer".into(),
            binding_name: "weather-db".into(),
            driver: "gcp-firestore".into(),
            store_id: "fds-tenant-secret-weather".into(),
            workload_id: "workload-secret".into(),
            read: true,
            write: true,
            lease_seconds: None,
        };
        let id = secret_id("tenant-secret", &request);
        assert_eq!(id, secret_id("tenant-secret", &request));
        assert!(!id.contains("tenant-secret"));
        assert!(!id.contains("weather"));
        assert!(!id.contains("workload"));
    }
}
