//! One Firestore codec and compare-and-swap boundary for policy writers and GC.
//!
//! Retirement is permanent for an incarnation. An identity-only document fences
//! concurrent first-time policy creation too. Absence is separate from retirement:
//! a failed or asynchronous provider deletion must never start retention.

use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::driver::{ComputeError, Result};
use crate::payment_policy::{DeploymentIdentity, PaymentPolicy};

pub(crate) const RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;
pub(crate) const PAGE_SIZE: usize = 10;

#[derive(Clone)]
pub(crate) struct PolicyRepository {
    pub(crate) documents_url: String,
    client: reqwest::Client,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredPolicy {
    pub(crate) deployment: DeploymentIdentity,
    pub(crate) policy: Option<PaymentPolicy>,
    pub(crate) retired: bool,
    pub(crate) absent_since: Option<u64>,
    pub(crate) update_time: String,
}

pub(crate) struct ScanCheckpoint {
    pub(crate) page_token: Option<String>,
    update_time: Option<String>,
}

impl StoredPolicy {
    pub(crate) fn retire(&self) -> Result<Self> {
        let mut next = self.clone();
        if !next.retired {
            if let Some(policy) = &mut next.policy {
                policy.version = policy.version.checked_add(1).ok_or_else(|| {
                    ComputeError::Configuration("payment policy version exhausted".into())
                })?;
                policy.deleted = true;
            }
            next.retired = true;
        }
        Ok(next)
    }
}

impl PolicyRepository {
    pub(crate) fn new(documents_url: String) -> Result<Self> {
        Ok(Self {
            documents_url,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    pub(crate) fn key(deployment: &DeploymentIdentity) -> String {
        let key = Sha256::digest(format!(
            "{}\0{}\0{}",
            deployment.owner_key, deployment.resource_name, deployment.created_at
        ));
        key.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn document_url(&self, deployment: &DeploymentIdentity) -> String {
        format!("{}/{}", self.documents_url, Self::key(deployment))
    }

    pub(crate) fn decode(value: &Value) -> Result<StoredPolicy> {
        let policy: Option<PaymentPolicy> = match value.pointer("/fields/policy") {
            None => None,
            Some(value) => Some(serde_json::from_str(
                value
                    .get("stringValue")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_document)?,
            )?),
        };
        let deployment = match value.pointer("/fields/identity/stringValue") {
            Some(Value::String(identity)) => serde_json::from_str(identity)?,
            None => policy
                .as_ref()
                .map(|policy| policy.deployment.clone())
                .ok_or_else(invalid_document)?,
            _ => return Err(invalid_document()),
        };
        if policy
            .as_ref()
            .is_some_and(|policy| policy.deployment != deployment || policy.version == 0)
        {
            return Err(invalid_document());
        }
        let retired = match value.pointer("/fields/retired") {
            None => false,
            Some(value) => value
                .get("booleanValue")
                .and_then(Value::as_bool)
                .ok_or_else(invalid_document)?,
        };
        let absent_since = match value.pointer("/fields/absent_since") {
            None => None,
            Some(value) => Some(
                value
                    .get("integerValue")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(invalid_document)?,
            ),
        };
        if (retired && policy.as_ref().is_some_and(|policy| !policy.deleted))
            || (!retired && (policy.is_none() || absent_since.is_some()))
        {
            return Err(invalid_document());
        }
        let update_time = value
            .get("updateTime")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(invalid_document)?
            .to_owned();
        Ok(StoredPolicy {
            deployment,
            policy,
            retired,
            absent_since,
            update_time,
        })
    }

    pub(crate) async fn load(
        &self,
        token: &str,
        deployment: &DeploymentIdentity,
    ) -> Result<Option<StoredPolicy>> {
        let response = self
            .client
            .get(self.document_url(deployment))
            .bearer_auth(token)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let value: Value = check(response).await?.json().await?;
        let stored = Self::decode(&value)?;
        if &stored.deployment != deployment {
            return Err(invalid_document());
        }
        Ok(Some(stored))
    }

    /// All mutations use the same precondition, including tombstones and purge.
    pub(crate) async fn write(
        &self,
        token: &str,
        previous: Option<&StoredPolicy>,
        next: &StoredPolicy,
    ) -> Result<()> {
        if previous.is_some_and(|previous| {
            previous.deployment != next.deployment
                || (previous.retired && !next.retired)
                || previous.policy.as_ref().is_some_and(|old| {
                    next.policy
                        .as_ref()
                        .is_none_or(|new| new.version < old.version)
                })
        }) {
            return Err(invalid_document());
        }
        let mut fields = json!({
            "identity": {"stringValue": serde_json::to_string(&next.deployment)?},
            "retired": {"booleanValue": next.retired}
        });
        if let Some(policy) = &next.policy {
            fields["policy"] = json!({"stringValue": serde_json::to_string(policy)?});
        }
        if let Some(absent_since) = next.absent_since {
            fields["absent_since"] = json!({"integerValue": absent_since.to_string()});
        }
        let outgoing = self
            .client
            .patch(self.document_url(&next.deployment))
            .bearer_auth(token);
        let outgoing = match previous {
            Some(previous) => {
                outgoing.query(&[("currentDocument.updateTime", previous.update_time.as_str())])
            }
            None => outgoing.query(&[("currentDocument.exists", "false")]),
        };
        check(outgoing.json(&json!({"fields": fields})).send().await?).await?;
        Ok(())
    }

    pub(crate) async fn purge(&self, token: &str, stored: &StoredPolicy) -> Result<()> {
        if !stored.retired || stored.absent_since.is_none() {
            return Err(invalid_document());
        }
        let response = self
            .client
            .delete(self.document_url(&stored.deployment))
            .bearer_auth(token)
            .query(&[("currentDocument.updateTime", stored.update_time.as_str())])
            .send()
            .await?;
        if response.status() != StatusCode::NOT_FOUND {
            check(response).await?;
        }
        Ok(())
    }

    pub(crate) async fn page(
        &self,
        token: &str,
        page_token: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let mut request = self
            .client
            .get(&self.documents_url)
            .bearer_auth(token)
            .query(&[("pageSize", PAGE_SIZE.to_string())]);
        if let Some(page_token) = page_token {
            request = request.query(&[("pageToken", page_token)]);
        }
        let value: Value = check(request.send().await?).await?.json().await?;
        let documents = match value.get("documents") {
            None => Vec::new(),
            Some(Value::Array(documents)) if documents.len() <= PAGE_SIZE => documents.clone(),
            _ => return Err(invalid_document()),
        };
        let next = value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        Ok((documents, next))
    }

    pub(crate) fn validate_name(&self, value: &Value, stored: &StoredPolicy) -> Result<()> {
        // Firestore's document name is relative to /v1/, not a URL. Never use
        // a returned document name as a request destination.
        let expected = self
            .document_url(&stored.deployment)
            .split_once("/v1/")
            .map(|(_, name)| name.to_owned())
            .ok_or_else(invalid_document)?;
        if value.get("name").and_then(Value::as_str) != Some(expected.as_str()) {
            return Err(invalid_document());
        }
        Ok(())
    }

    fn checkpoint_url(&self, scope: &str) -> Result<String> {
        let root = self
            .documents_url
            .rsplit_once('/')
            .ok_or_else(invalid_document)?
            .0;
        let digest = Sha256::digest(scope.as_bytes());
        let key: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(format!("{root}/pay_compute_policy_reconciliation/{key}"))
    }

    pub(crate) async fn checkpoint(&self, token: &str, scope: &str) -> Result<ScanCheckpoint> {
        let response = self
            .client
            .get(self.checkpoint_url(scope)?)
            .bearer_auth(token)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(ScanCheckpoint {
                page_token: None,
                update_time: None,
            });
        }
        let value: Value = check(response).await?.json().await?;
        let page_token = value
            .pointer("/fields/page_token/stringValue")
            .and_then(Value::as_str)
            .ok_or_else(invalid_document)?;
        let update_time = value
            .get("updateTime")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(invalid_document)?;
        Ok(ScanCheckpoint {
            page_token: (!page_token.is_empty()).then(|| page_token.to_owned()),
            update_time: Some(update_time.to_owned()),
        })
    }

    pub(crate) async fn advance_checkpoint(
        &self,
        token: &str,
        scope: &str,
        checkpoint: &mut ScanCheckpoint,
        next_page: Option<String>,
    ) -> Result<()> {
        let outgoing = self
            .client
            .patch(self.checkpoint_url(scope)?)
            .bearer_auth(token);
        let outgoing = match &checkpoint.update_time {
            Some(time) => outgoing.query(&[("currentDocument.updateTime", time.as_str())]),
            None => outgoing.query(&[("currentDocument.exists", "false")]),
        };
        let response: Value = check(
            outgoing
                .json(&json!({
                    "fields": { "page_token": {"stringValue": next_page.as_deref().unwrap_or("")}}
                }))
                .send()
                .await?,
        )
        .await?
        .json()
        .await?;
        checkpoint.update_time = Some(
            response
                .get("updateTime")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(invalid_document)?
                .to_owned(),
        );
        checkpoint.page_token = next_page;
        Ok(())
    }
}

fn invalid_document() -> ComputeError {
    ComputeError::Configuration("stored payment policy is invalid".into())
}

async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.json::<Value>().await.ok();
    let code = body
        .as_ref()
        .and_then(|value| value.pointer("/error/status"))
        .and_then(Value::as_str);
    if matches!(
        status,
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED
    ) || matches!(
        code,
        Some("FAILED_PRECONDITION" | "ABORTED" | "ALREADY_EXISTS")
    ) {
        return Err(ComputeError::InvalidRequest(
            "payment policy version conflict; retry with current version".into(),
        ));
    }
    Err(ComputeError::Provider(
        "payment policy storage unavailable".into(),
    ))
}
