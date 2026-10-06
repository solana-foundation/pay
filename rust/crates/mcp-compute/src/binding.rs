use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::driver::{ComputeError, Result};
use crate::google::GoogleCloudFunctionsDriver;
use crate::types::{ServiceBindingSpec, Tenant};

const INTERNAL_PROOF_HEADER: &str = "x-pay-data-binding-proof";

#[derive(Clone)]
pub struct DataBindingClient {
    base_url: Arc<str>,
    internal_proof: Arc<str>,
    google: GoogleCloudFunctionsDriver,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct BindingRequest<'a> {
    payer: &'a str,
    binding_name: &'a str,
    driver: &'a str,
    store_id: &'a str,
    workload_id: &'a str,
    read: bool,
    write: bool,
    lease_seconds: Option<u32>,
}

#[derive(Deserialize)]
struct BindingResponse {
    url_path: String,
    secret_id: String,
    expires_at: u64,
}

pub struct ResolvedBindings {
    pub environment: BTreeMap<String, String>,
    pub secrets: BTreeMap<String, String>,
}

impl DataBindingClient {
    pub fn from_env(google: GoogleCloudFunctionsDriver) -> Result<Option<Self>> {
        let Some(base_url) = env_nonempty("COMPUTE_DATA_SERVICE_URL") else {
            return Ok(None);
        };
        let internal_proof = env_nonempty("COMPUTE_DATA_BINDING_INTERNAL_PROOF").ok_or_else(|| {
            ComputeError::Configuration(
                "COMPUTE_DATA_BINDING_INTERNAL_PROOF is required when COMPUTE_DATA_SERVICE_URL is set"
                    .into(),
            )
        })?;
        let base_url = base_url.trim_end_matches('/').to_string();
        url::Url::parse(&base_url).map_err(|_| {
            ComputeError::Configuration("COMPUTE_DATA_SERVICE_URL must be an absolute URL".into())
        })?;
        Ok(Some(Self {
            base_url: base_url.into(),
            internal_proof: internal_proof.into(),
            google,
            client: reqwest::Client::new(),
        }))
    }

    pub async fn resolve(
        &self,
        tenant: &Tenant,
        workload_id: &str,
        bindings: &[ServiceBindingSpec],
    ) -> Result<ResolvedBindings> {
        let mut environment = BTreeMap::new();
        let mut secrets = BTreeMap::new();
        let mut names = BTreeSet::new();
        for binding in bindings {
            let env_name = binding_environment_name(&binding.name)?;
            if !names.insert(env_name.clone()) {
                return Err(ComputeError::InvalidRequest(format!(
                    "duplicate service binding name `{}`",
                    binding.name
                )));
            }
            if binding.service.kind != "document_store" {
                return Err(ComputeError::InvalidRequest(format!(
                    "unsupported service binding kind `{}`; valid kinds: document_store",
                    binding.service.kind
                )));
            }
            if !binding.read && !binding.write {
                return Err(ComputeError::InvalidRequest(format!(
                    "service binding `{}` must grant read, write, or both",
                    binding.name
                )));
            }
            let request = BindingRequest {
                payer: &tenant.payer,
                binding_name: &binding.name,
                driver: &binding.service.driver,
                store_id: &binding.service.id,
                workload_id,
                read: binding.read,
                write: binding.write,
                lease_seconds: binding.lease_seconds,
            };
            let mut outgoing = self
                .client
                .post(format!("{}/__402/bind", self.base_url))
                .header(INTERNAL_PROOF_HEADER, self.internal_proof.as_ref())
                .json(&request);
            if let Some(token) = self.google.identity_token(&self.base_url).await? {
                outgoing = outgoing.bearer_auth(token);
            }
            let response = outgoing.send().await?;
            let status = response.status();
            if !status.is_success() {
                return Err(binding_error(status));
            }
            let response: BindingResponse = response.json().await?;
            if !response.url_path.starts_with("/__402/bindings/") {
                return Err(ComputeError::Provider(
                    "data binding service returned an invalid path".into(),
                ));
            }
            environment.insert(
                format!("PAY_BINDING_{env_name}_URL"),
                format!("{}{}", self.base_url, response.url_path),
            );
            secrets.insert(
                format!("PAY_BINDING_{env_name}_CAPABILITY"),
                response.secret_id,
            );
            environment.insert(
                format!("PAY_BINDING_{env_name}_EXPIRES_AT"),
                response.expires_at.to_string(),
            );
        }
        Ok(ResolvedBindings {
            environment,
            secrets,
        })
    }
}

fn binding_environment_name(name: &str) -> Result<String> {
    if name.is_empty()
        || name.len() > 48
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ComputeError::InvalidRequest(format!(
            "invalid service binding name `{name}`; use 1-48 ASCII letters, digits, `_`, or `-`"
        )));
    }
    Ok(name
        .chars()
        .map(|character| match character {
            'a'..='z' => character.to_ascii_uppercase(),
            '-' => '_',
            other => other,
        })
        .collect())
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn binding_error(status: StatusCode) -> ComputeError {
    ComputeError::Provider(format!(
        "data binding service rejected the binding request with {status}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_names_map_to_stable_environment_keys() {
        assert_eq!(
            binding_environment_name("weather-db").unwrap(),
            "WEATHER_DB"
        );
        assert!(binding_environment_name("bad/name").is_err());
    }
}
